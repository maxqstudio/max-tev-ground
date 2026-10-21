//! Streaming inference over one verified input copy into a canonical Parquet output.
//!
//! Each input row is turned into one output row from that row's own bound values. Rows whose
//! inputs are not usable are SKIPPED without calling the model; answers that do not satisfy the
//! declared outputs are INVALID_OUTPUT. Runtime failures stop the run. Memory is bounded by the
//! write batch and a fixed number of preview rows.

use std::{
    collections::VecDeque,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use arrow::{
    array::{ArrayRef, Float64Array, RecordBatch, StringArray, TimestampMicrosecondArray},
    datatypes::{DataType, Field, Schema, TimeUnit},
};
use chrono::DateTime;
use parquet::{arrow::ArrowWriter, file::properties::WriterProperties};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    http::LoopbackEndpoint,
    input::{self, InputTable, Row},
    ollama::{self, GenerationSettings, OutputValue},
    onnx::NativeOnnxModel,
    spec::{
        hex, InferenceColumn, InferenceColumnKind, InferenceSummary, OutputField, OutputKind,
        RowReason, RowStatus, RunStatistics,
    },
    ModelError,
};

const WRITE_BATCH_ROWS: usize = 4096;
const MAX_ROW_GROUP_ROWS: usize = 65_536;
pub const PREVIEW_ROWS: usize = 20;
const IO_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProgressPhase {
    Queued,
    VerifyingModel,
    VerifyingInput,
    Inferring,
    Finalizing,
}

/// Lock-free progress counters shared between the worker and status queries.
#[derive(Debug, Default)]
pub struct InferenceProgress {
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

impl InferenceProgress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            phase: match self.phase.load(Ordering::Relaxed) {
                1 => ProgressPhase::VerifyingModel,
                2 => ProgressPhase::VerifyingInput,
                3 => ProgressPhase::Inferring,
                4 => ProgressPhase::Finalizing,
                _ => ProgressPhase::Queued,
            },
            processed_rows: self.processed_rows.load(Ordering::Relaxed),
            total_rows: self.total_rows.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_phase(&self, phase: ProgressPhase, total_rows: u64) {
        let value = match phase {
            ProgressPhase::Queued => 0,
            ProgressPhase::VerifyingModel => 1,
            ProgressPhase::VerifyingInput => 2,
            ProgressPhase::Inferring => 3,
            ProgressPhase::Finalizing => 4,
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
pub struct PreviewValue {
    pub field: String,
    pub value: Option<f64>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PreviewRow {
    pub open_time_utc: String,
    pub decision_time_utc: String,
    pub row_status: String,
    pub reason_code: Option<String>,
    pub inputs: Vec<PreviewValue>,
    pub outputs: Vec<PreviewValue>,
}

/// Where one bound input field reads its value.
#[derive(Clone, Debug)]
pub(crate) enum BoundValue {
    Feature { value: usize, status: usize },
    DecisionInput { value: usize },
    Decision { column: usize },
}

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub fields: Vec<String>,
    pub bound: Vec<BoundValue>,
    pub table: InputTable,
}

/// The outcome of one row.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowOutcome {
    Ok(Vec<OutputValue>),
    Skipped(RowReason),
    Invalid(RowReason),
}

/// A model that answers one row at a time.
pub(crate) trait RowModel {
    fn infer(&mut self, inputs: &[f64]) -> Result<RowOutcome, ModelError>;
}

pub(crate) struct OnnxRows<'a> {
    pub model: &'a NativeOnnxModel,
    pub outputs: &'a [OutputField],
}

impl RowModel for OnnxRows<'_> {
    fn infer(&mut self, inputs: &[f64]) -> Result<RowOutcome, ModelError> {
        let narrowed = inputs.iter().map(|value| *value as f32).collect::<Vec<_>>();
        if narrowed.iter().any(|value| !value.is_finite()) {
            return Ok(RowOutcome::Skipped(RowReason::NonFiniteInput));
        }
        let values = self.model.run(&narrowed)?;
        if values.len() != self.outputs.len() {
            return Ok(RowOutcome::Invalid(RowReason::SchemaMismatch));
        }
        let mut result = Vec::with_capacity(values.len());
        for (value, output) in values.iter().zip(self.outputs) {
            let value = f64::from(*value);
            if !value.is_finite() {
                return Ok(RowOutcome::Invalid(RowReason::NonFiniteOutput));
            }
            match &output.kind {
                OutputKind::Float64 { minimum, maximum } => {
                    if value < *minimum || value > *maximum {
                        return Ok(RowOutcome::Invalid(RowReason::OutOfRange));
                    }
                    result.push(OutputValue::Number(value));
                }
                OutputKind::Category { .. } => {
                    return Ok(RowOutcome::Invalid(RowReason::SchemaMismatch))
                }
            }
        }
        Ok(RowOutcome::Ok(result))
    }
}

/// Generations between re-checks of the registered Ollama digest during one run.
pub(crate) const DIGEST_CHECK_INTERVAL: u64 = 16;

pub(crate) struct OllamaRows<'a> {
    pub endpoint: LoopbackEndpoint,
    pub model_id: &'a str,
    pub model_name: &'a str,
    pub registered_digest: &'a str,
    pub settings: GenerationSettings,
    pub fields: &'a [String],
    pub outputs: &'a [OutputField],
    pub timeout: Duration,
    pub generations: u64,
}

impl RowModel for OllamaRows<'_> {
    fn infer(&mut self, inputs: &[f64]) -> Result<RowOutcome, ModelError> {
        let prompt = ollama::prompt(&self.settings.instruction, self.fields, inputs);
        let answer = ollama::generate(
            &self.endpoint,
            self.model_name,
            &self.settings,
            self.outputs,
            &prompt,
            self.timeout,
        )?;
        self.generations += 1;
        if self.generations.is_multiple_of(DIGEST_CHECK_INTERVAL) {
            ollama::verify_registered_digest(
                &self.endpoint,
                self.model_id,
                self.model_name,
                self.registered_digest,
                "during inference",
            )?;
        }
        Ok(match ollama::parse_output(&answer, self.outputs) {
            Ok(values) => RowOutcome::Ok(values),
            Err(reason) => RowOutcome::Invalid(reason),
        })
    }
}

pub(crate) struct Materialized {
    pub row_count: u64,
    pub output_sha256: String,
    pub output_fingerprint: String,
    pub columns: Vec<InferenceColumn>,
    pub summary: InferenceSummary,
    pub run: RunStatistics,
    pub preview_rows: Vec<PreviewRow>,
}

pub(crate) fn output_columns(fields: &[String], outputs: &[OutputField]) -> Vec<InferenceColumn> {
    let column = |name: String, kind| InferenceColumn { name, kind };
    let mut columns = vec![
        column("symbol".into(), InferenceColumnKind::Text),
        column("timeframe".into(), InferenceColumnKind::Text),
        column("open_time_utc".into(), InferenceColumnKind::TimestampUtc),
        column(
            "decision_time_utc".into(),
            InferenceColumnKind::TimestampUtc,
        ),
        column("row_status".into(), InferenceColumnKind::Text),
        column("reason_code".into(), InferenceColumnKind::NullableText),
    ];
    for field in fields {
        columns.push(column(
            format!("input__{field}"),
            InferenceColumnKind::NullableFloat64,
        ));
    }
    for output in outputs {
        let kind = match output.kind {
            OutputKind::Float64 { .. } => InferenceColumnKind::NullableFloat64,
            OutputKind::Category { .. } => InferenceColumnKind::NullableText,
        };
        columns.push(column(format!("output__{}", output.name), kind));
    }
    columns
}

fn timestamp_type() -> DataType {
    DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
}

pub(crate) fn output_schema(columns: &[InferenceColumn]) -> Schema {
    Schema::new(
        columns
            .iter()
            .map(|column| match column.kind {
                InferenceColumnKind::Text => Field::new(&column.name, DataType::Utf8, false),
                InferenceColumnKind::NullableText => Field::new(&column.name, DataType::Utf8, true),
                InferenceColumnKind::TimestampUtc => {
                    Field::new(&column.name, timestamp_type(), false)
                }
                InferenceColumnKind::NullableFloat64 => {
                    Field::new(&column.name, DataType::Float64, true)
                }
            })
            .collect::<Vec<_>>(),
    )
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// One output row, ready for the writer and the preview.
struct OutputRow {
    open_us: i64,
    decision_us: i64,
    status: RowStatus,
    reason: Option<RowReason>,
    inputs: Vec<Option<f64>>,
    outputs: Vec<Option<OutputValue>>,
}

enum OutputBuffer {
    Numbers(Vec<Option<f64>>),
    Labels(Vec<Option<String>>),
}

struct OutputWriter {
    writer: ArrowWriter<File>,
    schema: Arc<Schema>,
    symbol: String,
    timeframe: String,
    open_times: Vec<i64>,
    decision_times: Vec<i64>,
    statuses: Vec<&'static str>,
    reasons: Vec<Option<&'static str>>,
    inputs: Vec<Vec<Option<f64>>>,
    outputs: Vec<OutputBuffer>,
    hasher: Sha256,
    max_output_bytes: u64,
    path: PathBuf,
}

impl OutputWriter {
    fn create(
        path: &Path,
        columns: &[InferenceColumn],
        fields: usize,
        outputs: &[OutputField],
        symbol: &str,
        timeframe: &str,
        max_output_bytes: u64,
    ) -> Result<Self, ModelError> {
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
            symbol: symbol.into(),
            timeframe: timeframe.into(),
            open_times: Vec::with_capacity(WRITE_BATCH_ROWS),
            decision_times: Vec::with_capacity(WRITE_BATCH_ROWS),
            statuses: Vec::with_capacity(WRITE_BATCH_ROWS),
            reasons: Vec::with_capacity(WRITE_BATCH_ROWS),
            inputs: vec![Vec::with_capacity(WRITE_BATCH_ROWS); fields],
            outputs: outputs
                .iter()
                .map(|output| match output.kind {
                    OutputKind::Float64 { .. } => OutputBuffer::Numbers(Vec::new()),
                    OutputKind::Category { .. } => OutputBuffer::Labels(Vec::new()),
                })
                .collect(),
            hasher: Sha256::new(),
            max_output_bytes,
            path: path.to_path_buf(),
        })
    }

    /// Appends a row and folds it into the output fingerprint, column by column in schema order.
    fn push(&mut self, row: &OutputRow) -> Result<(), ModelError> {
        hash_field(&mut self.hasher, self.symbol.as_bytes());
        hash_field(&mut self.hasher, self.timeframe.as_bytes());
        self.hasher.update(row.open_us.to_be_bytes());
        self.hasher.update(row.decision_us.to_be_bytes());
        hash_field(&mut self.hasher, row.status.as_str().as_bytes());
        match row.reason {
            Some(reason) => {
                self.hasher.update([1]);
                hash_field(&mut self.hasher, reason.as_str().as_bytes());
            }
            None => self.hasher.update([0]),
        }
        for (column, value) in self.inputs.iter_mut().zip(&row.inputs) {
            match value {
                Some(value) => {
                    self.hasher.update([1]);
                    self.hasher.update(value.to_bits().to_be_bytes());
                }
                None => self.hasher.update([0]),
            }
            column.push(*value);
        }
        for (buffer, value) in self.outputs.iter_mut().zip(&row.outputs) {
            match (buffer, value) {
                (OutputBuffer::Numbers(values), Some(OutputValue::Number(number))) => {
                    self.hasher.update([1]);
                    self.hasher.update(number.to_bits().to_be_bytes());
                    values.push(Some(*number));
                }
                (OutputBuffer::Labels(values), Some(OutputValue::Label(label))) => {
                    self.hasher.update([1]);
                    hash_field(&mut self.hasher, label.as_bytes());
                    values.push(Some(label.clone()));
                }
                (OutputBuffer::Numbers(values), None) => {
                    self.hasher.update([0]);
                    values.push(None);
                }
                (OutputBuffer::Labels(values), None) => {
                    self.hasher.update([0]);
                    values.push(None);
                }
                _ => {
                    return Err(ModelError::InvalidPrepared(
                        "an output value does not match its declared kind".into(),
                    ))
                }
            }
        }
        self.open_times.push(row.open_us);
        self.decision_times.push(row.decision_us);
        self.statuses.push(row.status.as_str());
        self.reasons.push(row.reason.map(RowReason::as_str));
        if self.open_times.len() >= WRITE_BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ModelError> {
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
            Arc::new(StringArray::from(std::mem::take(&mut self.statuses))),
            Arc::new(StringArray::from(std::mem::take(&mut self.reasons))),
        ];
        for column in &mut self.inputs {
            arrays.push(Arc::new(Float64Array::from(std::mem::take(column))));
        }
        for buffer in &mut self.outputs {
            arrays.push(match buffer {
                OutputBuffer::Numbers(values) => {
                    Arc::new(Float64Array::from(std::mem::take(values)))
                }
                OutputBuffer::Labels(values) => Arc::new(StringArray::from(std::mem::take(values))),
            });
        }
        let batch = RecordBatch::try_new(Arc::clone(&self.schema), arrays)?;
        self.writer.write(&batch)?;
        let pending = self.writer.bytes_written() as u64 + self.writer.in_progress_size() as u64;
        if pending > self.max_output_bytes {
            return Err(ModelError::ResourceQuota(format!(
                "inference output exceeded the configured {} byte quota",
                self.max_output_bytes
            )));
        }
        Ok(())
    }

    /// Closes the file and enforces the quota on its final length, footer included.
    fn finish(mut self) -> Result<String, ModelError> {
        self.flush()?;
        self.writer.close()?;
        let final_bytes = std::fs::metadata(&self.path)?.len();
        if final_bytes > self.max_output_bytes {
            return Err(ModelError::ResourceQuota(format!(
                "inference output is {final_bytes} bytes including its footer; the quota is {}",
                self.max_output_bytes
            )));
        }
        Ok(hex(self.hasher.finalize()))
    }
}

/// The value one binding contributes to a row, or why the row is skipped.
fn bound_value(row: &Row<'_>, bound: &BoundValue) -> Result<Result<f64, RowReason>, String> {
    match bound {
        BoundValue::Feature { value, status } => {
            match row.text(*status) {
                Some("READY") => {}
                Some("WARMUP") => return Ok(Err(RowReason::Warmup)),
                Some("STALE") => return Ok(Err(RowReason::Stale)),
                Some("NO_OBSERVATION") => return Ok(Err(RowReason::NoObservation)),
                other => return Err(format!("unrecognized feature status {other:?}")),
            }
            Ok(row
                .float(*value)
                .filter(|value| value.is_finite())
                .ok_or(RowReason::NonFiniteInput))
        }
        BoundValue::DecisionInput { value } => Ok(row
            .float(*value)
            .filter(|value| value.is_finite())
            .ok_or(RowReason::NonFiniteInput)),
        BoundValue::Decision { column } => match row.text(*column) {
            Some("BUY") => Ok(Ok(1.0)),
            Some("SELL") => Ok(Ok(-1.0)),
            Some("SKIP") => Ok(Ok(0.0)),
            other => Err(format!("unrecognized decision {other:?}")),
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize(
    plan: &Plan,
    outputs: &[OutputField],
    model: &mut dyn RowModel,
    input_path: &Path,
    output_path: &Path,
    symbol: &str,
    timeframe: &str,
    max_rows: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
    progress: &InferenceProgress,
    input_id: &str,
) -> Result<Materialized, ModelError> {
    let integrity = |reason: String| ModelError::InputIntegrity {
        input_id: input_id.into(),
        reason,
    };
    if plan.table.row_count > max_rows {
        return Err(ModelError::ResourceQuota(format!(
            "input has {} rows; the inference-row quota is {max_rows}",
            plan.table.row_count
        )));
    }
    progress.set_phase(ProgressPhase::Inferring, plan.table.row_count);
    let columns = output_columns(&plan.fields, outputs);
    let mut writer = OutputWriter::create(
        output_path,
        &columns,
        plan.fields.len(),
        outputs,
        symbol,
        timeframe,
        max_output_bytes,
    )?;
    let mut summary = InferenceSummary::default();
    let mut run = RunStatistics::default();
    let mut preview = VecDeque::with_capacity(PREVIEW_ROWS);
    let mut row_count = 0u64;
    let read = input::read_rows(input_path, &plan.table, |row| -> Result<(), ModelError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        let open_us = row.time(plan.table.open_time_column);
        let decision_us = row.time(plan.table.decision_time_column);
        let mut values = Vec::with_capacity(plan.bound.len());
        let mut skip = None;
        for bound in &plan.bound {
            match bound_value(row, bound).map_err(integrity)? {
                Ok(value) => values.push(Some(value)),
                Err(reason) => {
                    skip.get_or_insert(reason);
                    values.push(None);
                }
            }
        }
        let outcome = match skip {
            Some(reason) => RowOutcome::Skipped(reason),
            None => {
                let inputs = values
                    .iter()
                    .map(|value| value.unwrap_or_default())
                    .collect::<Vec<_>>();
                let started = Instant::now();
                let outcome = model.infer(&inputs)?;
                let latency = started.elapsed().as_micros() as u64;
                if !matches!(outcome, RowOutcome::Skipped(_)) {
                    run.invoked_rows += 1;
                    run.total_latency_us = run.total_latency_us.saturating_add(latency);
                    run.max_latency_us = run.max_latency_us.max(latency);
                }
                outcome
            }
        };
        let (status, reason, output_values) = match outcome {
            RowOutcome::Ok(values) => (RowStatus::Ok, None, values.into_iter().map(Some).collect()),
            RowOutcome::Skipped(reason) => {
                (RowStatus::Skipped, Some(reason), vec![None; outputs.len()])
            }
            RowOutcome::Invalid(reason) => (
                RowStatus::InvalidOutput,
                Some(reason),
                vec![None; outputs.len()],
            ),
        };
        let output_row = OutputRow {
            open_us,
            decision_us,
            status,
            reason,
            inputs: values,
            outputs: output_values,
        };
        writer.push(&output_row)?;
        match status {
            RowStatus::Ok => summary.ok_rows += 1,
            RowStatus::Skipped => summary.skipped_rows += 1,
            RowStatus::InvalidOutput => summary.invalid_output_rows += 1,
        }
        if let Some(reason) = reason {
            *summary.reasons.entry(reason.as_str().into()).or_default() += 1;
        }
        summary
            .first_decision_utc
            .get_or_insert_with(|| format_utc(decision_us));
        summary.last_decision_utc = Some(format_utc(decision_us));
        if preview.len() == PREVIEW_ROWS {
            preview.pop_front();
        }
        preview.push_back(preview_row(&output_row, &plan.fields, outputs));
        row_count += 1;
        progress.add(1);
        Ok(())
    })
    .map_err(integrity)?;
    read?;
    progress.set_phase(ProgressPhase::Finalizing, 0);
    let output_fingerprint = writer.finish()?;
    let output_sha256 = sha256_file(output_path, cancel)?;
    Ok(Materialized {
        row_count,
        output_sha256,
        output_fingerprint,
        columns,
        summary,
        run,
        preview_rows: preview.into_iter().collect(),
    })
}

fn preview_row(row: &OutputRow, fields: &[String], outputs: &[OutputField]) -> PreviewRow {
    PreviewRow {
        open_time_utc: format_utc(row.open_us),
        decision_time_utc: format_utc(row.decision_us),
        row_status: row.status.as_str().into(),
        reason_code: row.reason.map(|reason| reason.as_str().into()),
        inputs: fields
            .iter()
            .zip(&row.inputs)
            .map(|(field, value)| PreviewValue {
                field: field.clone(),
                value: *value,
                label: None,
            })
            .collect(),
        outputs: outputs
            .iter()
            .zip(&row.outputs)
            .map(|(output, value)| PreviewValue {
                field: output.name.clone(),
                value: match value {
                    Some(OutputValue::Number(number)) => Some(*number),
                    _ => None,
                },
                label: match value {
                    Some(OutputValue::Label(label)) => Some(label.clone()),
                    _ => None,
                },
            })
            .collect(),
    }
}

pub(crate) fn format_utc(microseconds: i64) -> String {
    DateTime::from_timestamp_micros(microseconds)
        .unwrap_or(DateTime::UNIX_EPOCH)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

pub(crate) fn sha256_file(path: &Path, cancel: &AtomicBool) -> Result<String, ModelError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex(hasher.finalize()))
}

/// Copies `source` to a new private file while hashing it, at most `max_bytes` long. Returns the
/// copy's SHA-256 and length. The copy is what is read, so the bytes used are the bytes hashed.
pub(crate) fn copy_hashed(
    source: &Path,
    destination: &Path,
    max_bytes: u64,
    cancel: &AtomicBool,
) -> Result<(String, u64), ModelError> {
    use std::io::Write;
    let mut input = File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; IO_BUFFER_BYTES];
    let mut total = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > max_bytes {
            return Err(ModelError::ResourceQuota(format!(
                "file exceeds the {max_bytes} byte limit"
            )));
        }
        hasher.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    output.sync_all()?;
    Ok((hex(hasher.finalize()), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_resource_cancel_recovery_inference_progress_phases_are_observable() {
        let progress = InferenceProgress::default();
        assert_eq!(progress.snapshot().phase, ProgressPhase::Queued);
        progress.set_phase(ProgressPhase::Inferring, 10);
        progress.add(4);
        assert_eq!(
            progress.snapshot(),
            ProgressSnapshot {
                phase: ProgressPhase::Inferring,
                processed_rows: 4,
                total_rows: 10
            }
        );
    }
}
