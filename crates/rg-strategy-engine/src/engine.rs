//! Streaming decision evaluation over one verified RG-R02 feature-set output.
//!
//! The feature output is read in bounded record batches from a private staged copy whose SHA-256
//! already matched the feature-set manifest. While deciding, the engine recomputes the RG-R02 row
//! fingerprint from the exact rows it reads, so every value a decision used is the registered
//! content. Each row is decided from that row alone; memory is bounded by the batch size and a
//! fixed number of preview rows, not by series length.

use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Write},
    path::Path,
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
    arrow::{arrow_reader::ParquetRecordBatchReaderBuilder, ArrowWriter},
    file::properties::WriterProperties,
};
use rg_feature_studio::{spec::ColumnKind, FeatureSetManifest};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    spec::{
        decide, hex, DecisionColumn, DecisionColumnKind, DecisionSummary, RowInput,
        ValidatedRequest,
    },
    StrategyError,
};

const READ_BATCH_ROWS: usize = 8192;
const WRITE_BATCH_ROWS: usize = 8192;
const MAX_ROW_GROUP_ROWS: usize = 65_536;
pub const PREVIEW_ROWS: usize = 20;
const IO_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProgressPhase {
    Queued,
    VerifyingInput,
    Evaluating,
    Finalizing,
}

/// Lock-free progress counters shared between the worker and status queries.
#[derive(Debug, Default)]
pub struct DecisionProgress {
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

impl DecisionProgress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            phase: match self.phase.load(Ordering::Relaxed) {
                1 => ProgressPhase::VerifyingInput,
                2 => ProgressPhase::Evaluating,
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
            ProgressPhase::VerifyingInput => 1,
            ProgressPhase::Evaluating => 2,
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

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PreviewInput {
    pub role: String,
    pub status: String,
    pub value: Option<f64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PreviewDecision {
    pub open_time_utc: String,
    pub decision_time_utc: String,
    pub decision: String,
    pub reason_code: String,
    pub reason_role: Option<String>,
    pub inputs: Vec<PreviewInput>,
}

pub(crate) struct Materialized {
    pub row_count: u64,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub columns: Vec<DecisionColumn>,
    pub summary: DecisionSummary,
    pub preview_rows: Vec<PreviewDecision>,
}

/// Copies `source` to a new private file while hashing it, then checks the digest. The copy is
/// what the engine reads, so the bytes evaluated are exactly the bytes verified.
pub(crate) fn copy_verified(
    source: &Path,
    destination: &Path,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<bool, StrategyError> {
    let mut input = File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StrategyError::Cancelled);
        }
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    output.sync_all()?;
    Ok(hex(hasher.finalize()) == expected_sha256)
}

pub(crate) fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String, StrategyError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(StrategyError::Cancelled);
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex(hasher.finalize()))
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn timestamp_type() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
}

/// The Arrow field RG-R02 writes for each declared feature-set column.
fn feature_field(name: &str, kind: ColumnKind) -> Field {
    match kind {
        ColumnKind::TimestampUtc => Field::new(name, timestamp_type(), false),
        ColumnKind::NullableTimestampUtc => Field::new(name, timestamp_type(), true),
        ColumnKind::NullableFloat64 => Field::new(name, DataType::Float64, true),
        ColumnKind::Status => Field::new(name, DataType::Utf8, false),
    }
}

enum FeatureArray {
    Times(TimestampMicrosecondArray),
    Floats(Float64Array),
    Statuses(StringArray),
}

fn typed<T: Array + Clone + 'static>(batch: &RecordBatch, index: usize) -> Option<T> {
    batch.column(index).as_any().downcast_ref::<T>().cloned()
}

/// Column positions the evaluation needs, resolved once against the feature-set schema.
struct ColumnPlan {
    open_time: usize,
    decision_time: usize,
    bound: Vec<(usize, usize)>,
}

fn plan_columns(
    request: &ValidatedRequest,
    manifest: &FeatureSetManifest,
) -> Result<ColumnPlan, String> {
    let position = |name: &str| {
        manifest
            .columns
            .iter()
            .position(|column| column.name == name)
            .ok_or_else(|| format!("feature set has no column {name}"))
    };
    Ok(ColumnPlan {
        open_time: position("open_time_utc")?,
        decision_time: position("decision_time_utc")?,
        bound: request
            .columns
            .iter()
            .map(|bound| {
                Ok((
                    position(&bound.value_column)?,
                    position(&bound.status_column)?,
                ))
            })
            .collect::<Result<_, String>>()?,
    })
}

fn decision_columns(request: &ValidatedRequest) -> Vec<DecisionColumn> {
    let column = |name: &str, kind| DecisionColumn {
        name: name.into(),
        kind,
    };
    let mut columns = vec![
        column("symbol", DecisionColumnKind::Text),
        column("timeframe", DecisionColumnKind::Text),
        column("open_time_utc", DecisionColumnKind::TimestampUtc),
        column("decision_time_utc", DecisionColumnKind::TimestampUtc),
        column("decision", DecisionColumnKind::Text),
        column("reason_code", DecisionColumnKind::Text),
        column("reason_role", DecisionColumnKind::NullableText),
    ];
    for bound in &request.columns {
        columns.push(column(
            &format!("input__{}", bound.role),
            DecisionColumnKind::NullableFloat64,
        ));
    }
    columns
}

pub(crate) fn decision_schema(columns: &[DecisionColumn]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|column| match column.kind {
                DecisionColumnKind::Text => Field::new(&column.name, DataType::Utf8, false),
                DecisionColumnKind::NullableText => Field::new(&column.name, DataType::Utf8, true),
                DecisionColumnKind::TimestampUtc => {
                    Field::new(&column.name, timestamp_type(), false)
                }
                DecisionColumnKind::NullableFloat64 => {
                    Field::new(&column.name, DataType::Float64, true)
                }
            })
            .collect::<Vec<_>>(),
    )
}

/// One decided row, ready for the writer and the preview.
struct DecidedRow {
    open_us: i64,
    decision_us: i64,
    decision: &'static str,
    reason: &'static str,
    reason_role: Option<usize>,
    statuses: Vec<String>,
    values: Vec<Option<f64>>,
}

struct DecisionWriter {
    writer: ArrowWriter<File>,
    schema: Arc<Schema>,
    symbol: String,
    timeframe: String,
    roles: Vec<String>,
    open_times: Vec<i64>,
    decision_times: Vec<i64>,
    decisions: Vec<&'static str>,
    reasons: Vec<&'static str>,
    reason_roles: Vec<Option<String>>,
    inputs: Vec<Vec<Option<f64>>>,
    hasher: Sha256,
    max_output_bytes: u64,
}

impl DecisionWriter {
    fn create(
        path: &Path,
        columns: &[DecisionColumn],
        request: &ValidatedRequest,
        symbol: &str,
        timeframe: &str,
    ) -> Result<Self, StrategyError> {
        let schema = Arc::new(decision_schema(columns));
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
            symbol: symbol.into(),
            timeframe: timeframe.into(),
            roles: request
                .columns
                .iter()
                .map(|bound| bound.role.clone())
                .collect(),
            open_times: Vec::with_capacity(WRITE_BATCH_ROWS),
            decision_times: Vec::with_capacity(WRITE_BATCH_ROWS),
            decisions: Vec::with_capacity(WRITE_BATCH_ROWS),
            reasons: Vec::with_capacity(WRITE_BATCH_ROWS),
            reason_roles: Vec::with_capacity(WRITE_BATCH_ROWS),
            inputs: vec![Vec::with_capacity(WRITE_BATCH_ROWS); request.columns.len()],
            hasher: Sha256::new(),
            max_output_bytes: request.quotas.max_output_bytes,
        })
    }

    fn push(&mut self, row: &DecidedRow) -> Result<(), StrategyError> {
        let reason_role = row.reason_role.map(|index| self.roles[index].clone());
        hash_field(&mut self.hasher, self.symbol.as_bytes());
        hash_field(&mut self.hasher, self.timeframe.as_bytes());
        self.hasher.update(row.open_us.to_be_bytes());
        self.hasher.update(row.decision_us.to_be_bytes());
        hash_field(&mut self.hasher, row.decision.as_bytes());
        hash_field(&mut self.hasher, row.reason.as_bytes());
        match &reason_role {
            Some(role) => {
                self.hasher.update([1]);
                hash_field(&mut self.hasher, role.as_bytes());
            }
            None => self.hasher.update([0]),
        }
        for (column, value) in self.inputs.iter_mut().zip(&row.values) {
            match value {
                Some(value) => {
                    self.hasher.update([1]);
                    self.hasher.update(value.to_bits().to_be_bytes());
                }
                None => self.hasher.update([0]),
            }
            column.push(*value);
        }
        self.open_times.push(row.open_us);
        self.decision_times.push(row.decision_us);
        self.decisions.push(row.decision);
        self.reasons.push(row.reason);
        self.reason_roles.push(reason_role);
        if self.open_times.len() >= WRITE_BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StrategyError> {
        let rows = self.open_times.len();
        if rows == 0 {
            return Ok(());
        }
        let times = |values: &mut Vec<i64>| -> ArrayRef {
            Arc::new(TimestampMicrosecondArray::from(std::mem::take(values)).with_timezone("UTC"))
        };
        let mut arrays: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec![self.symbol.as_str(); rows])),
            Arc::new(StringArray::from(vec![self.timeframe.as_str(); rows])),
            times(&mut self.open_times),
            times(&mut self.decision_times),
            Arc::new(StringArray::from(std::mem::take(&mut self.decisions))),
            Arc::new(StringArray::from(std::mem::take(&mut self.reasons))),
            Arc::new(StringArray::from(std::mem::take(&mut self.reason_roles))),
        ];
        for column in &mut self.inputs {
            arrays.push(Arc::new(Float64Array::from(std::mem::take(column))));
        }
        let batch = RecordBatch::try_new(Arc::clone(&self.schema), arrays)?;
        self.writer.write(&batch)?;
        let pending = self.writer.bytes_written() as u64 + self.writer.in_progress_size() as u64;
        if pending > self.max_output_bytes {
            return Err(StrategyError::ResourceQuota(format!(
                "decision output exceeded the configured {} byte quota",
                self.max_output_bytes
            )));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<String, StrategyError> {
        self.flush()?;
        self.writer.close()?;
        Ok(hex(self.hasher.finalize()))
    }
}

/// Evaluates every row of the staged feature output and writes canonical decisions. Fails closed
/// if the rows read do not reproduce the feature set's registered row fingerprint and count.
pub(crate) fn materialize(
    request: &ValidatedRequest,
    feature_set: &FeatureSetManifest,
    input_path: &Path,
    output_path: &Path,
    cancel: &AtomicBool,
    progress: &DecisionProgress,
) -> Result<Materialized, StrategyError> {
    let integrity = |reason: String| StrategyError::FeatureSetIntegrity {
        feature_set_id: feature_set.feature_set_id.clone(),
        reason,
    };
    let plan = plan_columns(request, feature_set).map_err(integrity)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(input_path)?)
        .map_err(|error| integrity(format!("feature output is not readable Parquet: {error}")))?;
    let actual = builder.schema();
    let schema_matches = actual.fields().len() == feature_set.columns.len()
        && actual
            .fields()
            .iter()
            .zip(&feature_set.columns)
            .all(|(field, column)| {
                let expected = feature_field(&column.name, column.kind);
                field.name() == expected.name()
                    && field.data_type() == expected.data_type()
                    && field.is_nullable() == expected.is_nullable()
            });
    if !schema_matches {
        return Err(integrity(
            "feature output schema does not match its manifest".into(),
        ));
    }
    let reader = builder
        .with_batch_size(READ_BATCH_ROWS)
        .build()
        .map_err(|error| integrity(format!("feature output reader failed: {error}")))?;

    progress.set_phase(ProgressPhase::Evaluating, feature_set.row_count);
    let columns = decision_columns(request);
    let target = &feature_set.identity.target;
    let mut writer = DecisionWriter::create(
        output_path,
        &columns,
        request,
        &target.symbol,
        &target.timeframe,
    )?;
    let mut fingerprint = Sha256::new();
    let mut summary = DecisionSummary::default();
    let mut preview = VecDeque::with_capacity(PREVIEW_ROWS);
    let mut row_count = 0u64;
    let mut last_decision_us: Option<i64> = None;
    for batch in reader {
        let batch = batch.map_err(|error| integrity(format!("feature batch failed: {error}")))?;
        let arrays = feature_set
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let array = match column.kind {
                    ColumnKind::TimestampUtc | ColumnKind::NullableTimestampUtc => {
                        typed(&batch, index).map(FeatureArray::Times)
                    }
                    ColumnKind::NullableFloat64 => typed(&batch, index).map(FeatureArray::Floats),
                    ColumnKind::Status => typed(&batch, index).map(FeatureArray::Statuses),
                };
                array.ok_or_else(|| {
                    integrity(format!("column {} has an unexpected type", column.name))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            if row_count.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
                return Err(StrategyError::Cancelled);
            }
            if row_count >= request.quotas.max_rows {
                return Err(StrategyError::ResourceQuota(format!(
                    "feature set exceeded the configured {} decision-row quota",
                    request.quotas.max_rows
                )));
            }
            // Recompute RG-R02's row fingerprint over every column, in manifest order.
            for (array, column) in arrays.iter().zip(&feature_set.columns) {
                match (array, column.kind) {
                    (FeatureArray::Times(values), ColumnKind::TimestampUtc) => {
                        if values.is_null(row) {
                            return Err(integrity(format!("{} contains a null time", column.name)));
                        }
                        fingerprint.update(values.value(row).to_be_bytes());
                    }
                    (FeatureArray::Times(values), _) => {
                        if values.is_null(row) {
                            fingerprint.update([0]);
                        } else {
                            fingerprint.update([1]);
                            fingerprint.update(values.value(row).to_be_bytes());
                        }
                    }
                    (FeatureArray::Floats(values), _) => {
                        if values.is_null(row) {
                            fingerprint.update([0]);
                        } else {
                            fingerprint.update([1]);
                            fingerprint.update(values.value(row).to_bits().to_be_bytes());
                        }
                    }
                    (FeatureArray::Statuses(values), _) => {
                        if values.is_null(row) {
                            return Err(integrity(format!(
                                "{} contains a null status",
                                column.name
                            )));
                        }
                        hash_field(&mut fingerprint, values.value(row).as_bytes());
                    }
                }
            }
            let time = |index: usize| match &arrays[index] {
                FeatureArray::Times(values) => values.value(row),
                _ => unreachable!("planned time columns are timestamp columns"),
            };
            let (open_us, decision_us) = (time(plan.open_time), time(plan.decision_time));
            if last_decision_us.is_some_and(|last| decision_us <= last) {
                return Err(integrity(
                    "feature rows are not in strictly increasing decision-time order".into(),
                ));
            }
            last_decision_us = Some(decision_us);
            let inputs: Vec<RowInput<'_>> = plan
                .bound
                .iter()
                .map(|&(value_index, status_index)| {
                    let status = match &arrays[status_index] {
                        FeatureArray::Statuses(values) => values.value(row),
                        _ => unreachable!("planned status columns are status columns"),
                    };
                    let value = match &arrays[value_index] {
                        FeatureArray::Floats(values) => {
                            (!values.is_null(row)).then(|| values.value(row))
                        }
                        _ => unreachable!("planned value columns are float columns"),
                    };
                    RowInput { status, value }
                })
                .collect();
            let outcome = decide(&request.config, &inputs).ok_or_else(|| {
                integrity(format!(
                    "unrecognized feature status at {}",
                    format_utc(decision_us)
                ))
            })?;
            let decided = DecidedRow {
                open_us,
                decision_us,
                decision: outcome.decision.as_str(),
                reason: outcome.reason.as_str(),
                reason_role: outcome.role_index,
                statuses: inputs.iter().map(|input| input.status.to_owned()).collect(),
                values: inputs
                    .iter()
                    .map(|input| {
                        input
                            .value
                            .filter(|value| input.status == "READY" && value.is_finite())
                    })
                    .collect(),
            };
            writer.push(&decided)?;
            match outcome.decision {
                crate::spec::Decision::Buy => summary.buy_rows += 1,
                crate::spec::Decision::Sell => summary.sell_rows += 1,
                crate::spec::Decision::Skip => {
                    summary.skip_rows += 1;
                    *summary
                        .skip_reasons
                        .entry(outcome.reason.as_str().into())
                        .or_default() += 1;
                }
            }
            summary
                .first_decision_utc
                .get_or_insert_with(|| format_utc(decision_us));
            summary.last_decision_utc = Some(format_utc(decision_us));
            if preview.len() == PREVIEW_ROWS {
                preview.pop_front();
            }
            preview.push_back(preview_row(&decided, request));
            row_count += 1;
            progress.add(1);
        }
    }
    if row_count != feature_set.row_count
        || hex(fingerprint.finalize()) != feature_set.output_fingerprint
    {
        return Err(integrity(
            "rows read do not reproduce the registered row count and row fingerprint".into(),
        ));
    }
    progress.set_phase(ProgressPhase::Finalizing, 0);
    let output_fingerprint = writer.finish()?;
    let output_sha256 = sha256_file(output_path, cancel)?;
    Ok(Materialized {
        row_count,
        output_sha256,
        output_fingerprint,
        columns,
        summary,
        preview_rows: preview.into_iter().collect(),
    })
}

fn preview_row(row: &DecidedRow, request: &ValidatedRequest) -> PreviewDecision {
    PreviewDecision {
        open_time_utc: format_utc(row.open_us),
        decision_time_utc: format_utc(row.decision_us),
        decision: row.decision.into(),
        reason_code: row.reason.into(),
        reason_role: row
            .reason_role
            .map(|index| request.columns[index].role.clone()),
        inputs: request
            .columns
            .iter()
            .zip(row.statuses.iter().zip(&row.values))
            .map(|(bound, (status, value))| PreviewInput {
                role: bound.role.clone(),
                status: status.clone(),
                value: *value,
            })
            .collect(),
    }
}

pub(crate) fn format_utc(microseconds: i64) -> String {
    DateTime::from_timestamp_micros(microseconds)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_resource_cancel_recovery_progress_phases_are_observable() {
        let progress = DecisionProgress::default();
        assert_eq!(progress.snapshot().phase, ProgressPhase::Queued);
        progress.set_phase(ProgressPhase::Evaluating, 10);
        progress.add(4);
        assert_eq!(
            progress.snapshot(),
            ProgressSnapshot {
                phase: ProgressPhase::Evaluating,
                processed_rows: 4,
                total_rows: 10
            }
        );
        progress.set_phase(ProgressPhase::Finalizing, 0);
        assert_eq!(progress.snapshot().processed_rows, 0);
    }

    #[test]
    fn gate_strategy_input_authority_staged_copy_must_match_the_registered_digest() {
        let directory = tempfile::TempDir::new().unwrap();
        let source = directory.path().join("features.parquet");
        std::fs::write(&source, b"registered feature bytes").unwrap();
        let digest = sha256_file(&source, &AtomicBool::new(false)).unwrap();
        let never = AtomicBool::new(false);
        assert!(copy_verified(&source, &directory.path().join("a"), &digest, &never).unwrap());
        assert!(!copy_verified(
            &source,
            &directory.path().join("b"),
            &"0".repeat(64),
            &never
        )
        .unwrap());
        assert_eq!(
            std::fs::read(directory.path().join("a")).unwrap(),
            b"registered feature bytes"
        );
        assert!(matches!(
            copy_verified(
                &source,
                &directory.path().join("c"),
                &digest,
                &AtomicBool::new(true)
            ),
            Err(StrategyError::Cancelled)
        ));
    }

    const HOUR: i64 = 3_600_000_000;

    /// A one-feature RG-R02 output written by hand, with its row fingerprint computed here by an
    /// independent implementation of the RG-R02 scheme.
    struct HandFeature {
        directory: tempfile::TempDir,
        path: std::path::PathBuf,
        manifest: FeatureSetManifest,
    }

    fn hand_feature(rows: &[(i64, Option<f64>, &str)]) -> HandFeature {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("features.parquet");
        let schema = Arc::new(Schema::new(vec![
            feature_field("open_time_utc", ColumnKind::TimestampUtc),
            feature_field("close_time_utc", ColumnKind::TimestampUtc),
            feature_field("decision_time_utc", ColumnKind::TimestampUtc),
            feature_field("sig__value", ColumnKind::NullableFloat64),
            feature_field("sig__status", ColumnKind::Status),
        ]));
        let times = |offset: i64| -> ArrayRef {
            Arc::new(
                TimestampMicrosecondArray::from(
                    rows.iter().map(|row| row.0 + offset).collect::<Vec<_>>(),
                )
                .with_timezone("UTC"),
            )
        };
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                times(0),
                times(HOUR),
                times(HOUR),
                Arc::new(Float64Array::from(
                    rows.iter().map(|row| row.1).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter().map(|row| row.2).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap();
        let mut writer = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let mut hasher = Sha256::new();
        for (open, value, status) in rows {
            for time in [*open, open + HOUR, open + HOUR] {
                hasher.update(time.to_be_bytes());
            }
            match value {
                Some(value) => {
                    hasher.update([1]);
                    hasher.update(value.to_bits().to_be_bytes());
                }
                None => hasher.update([0]),
            }
            hasher.update((status.len() as u64).to_be_bytes());
            hasher.update(status.as_bytes());
        }
        let semantics = rg_feature_studio::TemporalSemantics::current();
        let manifest: FeatureSetManifest = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "feature_set_id": format!("fs_{}", "a".repeat(64)),
            "identity_sha256": "a".repeat(64),
            "identity": {
                "schema_version": 1, "engine_id": "rg-native-features", "engine_version": "1.0.0",
                "temporal_semantics": semantics,
                "target": {"dataset_id": format!("ds_{}", "b".repeat(64)), "symbol": "EURUSD.m", "timeframe": "H1"},
                "features": [{"key": "sig", "indicator_id": "ROC", "indicator_version": 1,
                              "parameters": {"period": 2}, "source": {"kind": "TARGET"},
                              "outputs": ["value"], "warmup_bars": 2}],
                "inputs": []
            },
            "created_at_utc": "2026-10-20T00:00:00.000000Z",
            "output_file": "features.parquet",
            "output_sha256": "c".repeat(64),
            "output_fingerprint": hex(hasher.finalize()),
            "row_count": rows.len(),
            "columns": [
                {"name": "open_time_utc", "kind": "TIMESTAMP_UTC", "feature_key": null},
                {"name": "close_time_utc", "kind": "TIMESTAMP_UTC", "feature_key": null},
                {"name": "decision_time_utc", "kind": "TIMESTAMP_UTC", "feature_key": null},
                {"name": "sig__value", "kind": "NULLABLE_FLOAT64", "feature_key": "sig"},
                {"name": "sig__status", "kind": "STATUS", "feature_key": "sig"}
            ],
            "feature_summaries": [],
            "lineage": [],
            "status": "READY",
            "research_notice": "fixture"
        }))
        .unwrap();
        HandFeature {
            directory,
            path,
            manifest,
        }
    }

    fn evaluate(fixture: &HandFeature, output: &str) -> Result<Materialized, StrategyError> {
        let request = crate::spec::DecisionSetRequest {
            feature_set_id: fixture.manifest.feature_set_id.clone(),
            strategy_id: "THRESHOLD_BAND".into(),
            strategy_version: 1,
            parameters: [("upper".to_owned(), 1.0), ("lower".to_owned(), -1.0)].into(),
            bindings: [(
                "signal".to_owned(),
                crate::spec::FeatureBinding {
                    feature_key: "sig".into(),
                    output: "value".into(),
                },
            )]
            .into(),
            quotas: crate::spec::DecisionQuotas {
                max_rows: 100,
                max_output_bytes: 1_000_000,
            },
        };
        let validated = crate::spec::validate_request(&request, &fixture.manifest)?;
        materialize(
            &validated,
            &fixture.manifest,
            &fixture.path,
            &fixture.directory.path().join(output),
            &AtomicBool::new(false),
            &DecisionProgress::default(),
        )
    }

    #[test]
    fn gate_strategy_input_authority_rows_must_reproduce_the_registered_fingerprint() {
        let rows = [
            (0, None, "WARMUP"),
            (HOUR, Some(2.5), "READY"),
            (2 * HOUR, Some(-3.0), "READY"),
            (3 * HOUR, Some(1.0), "READY"),
        ];
        let fixture = hand_feature(&rows);
        let materialized = evaluate(&fixture, "ok.parquet").unwrap();
        assert_eq!(materialized.row_count, 4);
        assert_eq!(
            materialized
                .preview_rows
                .iter()
                .map(|row| (row.decision.as_str(), row.reason_code.as_str()))
                .collect::<Vec<_>>(),
            [
                ("SKIP", "WARMUP"),
                ("BUY", "BUY_RULE"),
                ("SELL", "SELL_RULE"),
                ("SKIP", "NO_SIGNAL")
            ]
        );

        let integrity = |result: Result<Materialized, StrategyError>| {
            matches!(result, Err(StrategyError::FeatureSetIntegrity { .. }))
        };
        let mut wrong_fingerprint = hand_feature(&rows);
        wrong_fingerprint.manifest.output_fingerprint = "0".repeat(64);
        assert!(integrity(evaluate(&wrong_fingerprint, "a.parquet")));
        let mut wrong_count = hand_feature(&rows);
        wrong_count.manifest.row_count = 5;
        assert!(integrity(evaluate(&wrong_count, "b.parquet")));
        let unordered = hand_feature(&[(HOUR, Some(2.5), "READY"), (0, Some(2.5), "READY")]);
        assert!(integrity(evaluate(&unordered, "c.parquet")));
        let unknown_status = hand_feature(&[(0, Some(2.5), "FORWARD_FILLED")]);
        assert!(integrity(evaluate(&unknown_status, "d.parquet")));
        // An unbound column whose declared kind differs from the file fails the schema check; a
        // bound column with the wrong kind is already rejected by request validation.
        let mut wrong_schema = hand_feature(&rows);
        wrong_schema.manifest.columns[1].kind = ColumnKind::NullableTimestampUtc;
        assert!(integrity(evaluate(&wrong_schema, "e.parquet")));
        let mut wrong_bound_kind = hand_feature(&rows);
        wrong_bound_kind.manifest.columns[3].kind = ColumnKind::NullableTimestampUtc;
        assert!(matches!(
            evaluate(&wrong_bound_kind, "f.parquet"),
            Err(StrategyError::InvalidBinding { .. })
        ));
    }
}
