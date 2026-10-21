//! Descriptive comparison of inference records computed over the same input identity.
//!
//! Reports coverage, invalid-output rates, agreement between records, absolute differences
//! between their numeric outputs, and recorded latencies. It has no labels, prices, or positions,
//! so it reports no accuracy against outcomes, PnL, win rate, profitability, or edge.

use std::{
    collections::BTreeSet,
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    inference::copy_hashed,
    input::{self, ColumnType, InputTable},
    spec::{InferenceColumnKind, InferenceRecordManifest, OutputKind},
    store::ModelLabStore,
    ModelError,
};

pub const MAX_COMPARED_RECORDS: usize = 8;
pub const MAX_COMPARED_ROWS: u64 = 5_000_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparisonRequest {
    pub record_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RecordCoverage {
    pub record_id: String,
    pub model_id: String,
    pub model_name: String,
    pub rows: u64,
    pub ok_rows: u64,
    pub skipped_rows: u64,
    pub invalid_output_rows: u64,
    /// OK rows as a fraction of all rows.
    pub coverage: f64,
    /// INVALID_OUTPUT rows as a fraction of rows the model was asked to answer.
    pub invalid_output_rate: Option<f64>,
    pub mean_latency_us: Option<f64>,
    pub max_latency_us: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct FieldAgreement {
    pub field: String,
    pub kind: String,
    /// Rows where both records produced an OK value for this field.
    pub rows_both_ok: u64,
    /// Category fields: rows with the same label.
    pub agreeing_rows: Option<u64>,
    pub agreement_rate: Option<f64>,
    /// Numeric fields: absolute differences between the two records' values.
    pub mean_absolute_difference: Option<f64>,
    pub max_absolute_difference: Option<f64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PairComparison {
    pub left_record_id: String,
    pub right_record_id: String,
    pub fields: Vec<FieldAgreement>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ComparisonReport {
    pub input_kind: String,
    pub input_id: String,
    pub input_output_sha256: String,
    pub rows: u64,
    pub records: Vec<RecordCoverage>,
    pub pairs: Vec<PairComparison>,
    pub notice: String,
}

pub const COMPARISON_NOTICE: &str = "Descriptive statistics of model outputs over one shared input: coverage, invalid-output rate, agreement between models, numeric differences between models, and recorded latency. They are not accuracy against outcomes and not evidence of PnL, win rate, profitability, or edge.";

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Missing,
    Number(f64),
    Label(String),
}

struct LoadedRecord {
    manifest: InferenceRecordManifest,
    decision_times: Vec<i64>,
    ok: Vec<bool>,
    /// Indexed by declared output, then by row.
    values: Vec<Vec<Value>>,
}

fn column_type(kind: InferenceColumnKind) -> ColumnType {
    match kind {
        InferenceColumnKind::Text => ColumnType::Text,
        InferenceColumnKind::NullableText => ColumnType::NullableText,
        InferenceColumnKind::TimestampUtc => ColumnType::Timestamp,
        InferenceColumnKind::NullableFloat64 => ColumnType::Float,
    }
}

impl ModelLabStore {
    /// Compares two or more READY records over the same input identity. Each output file is
    /// copied privately, its digest checked against the manifest, and its row fingerprint
    /// recomputed while reading. Nothing is written to any registry.
    pub fn compare(
        &self,
        request: &ComparisonRequest,
        cancel: &AtomicBool,
    ) -> Result<ComparisonReport, ModelError> {
        let ids = request.record_ids.iter().collect::<BTreeSet<_>>();
        if request.record_ids.len() < 2
            || request.record_ids.len() > MAX_COMPARED_RECORDS
            || ids.len() != request.record_ids.len()
        {
            return Err(ModelError::ComparisonRefused(format!(
                "select between 2 and {MAX_COMPARED_RECORDS} different inference records"
            )));
        }
        let mut records = Vec::with_capacity(request.record_ids.len());
        let scratch = self.staging_root().join(Uuid::now_v7().to_string());
        fs::create_dir(&scratch)?;
        let result = (|| {
            for record_id in &request.record_ids {
                let (manifest, path) = self.usable_record(record_id)?;
                if let Some(first) = records.first() {
                    let first: &LoadedRecord = first;
                    if first.manifest.identity.input != manifest.identity.input {
                        return Err(ModelError::ComparisonRefused(format!(
                            "{record_id} was computed over a different input identity than {}",
                            first.manifest.record_id
                        )));
                    }
                }
                if manifest.row_count > MAX_COMPARED_ROWS {
                    return Err(ModelError::ResourceQuota(format!(
                        "comparison is limited to {MAX_COMPARED_ROWS} rows"
                    )));
                }
                let copy = scratch.join(format!("{}.parquet", records.len()));
                let (digest, _) = copy_hashed(&path, &copy, u64::MAX, cancel)?;
                if digest != manifest.output_sha256 {
                    return Err(ModelError::RecordNotUsable {
                        record_id: record_id.clone(),
                        status: "QUARANTINED".into(),
                    });
                }
                records.push(load(manifest, &copy, cancel)?);
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&scratch);
        result?;
        let rows = records[0].decision_times.len();
        if records
            .iter()
            .any(|record| record.decision_times != records[0].decision_times)
        {
            return Err(ModelError::ComparisonRefused(
                "records do not share the same decision times".into(),
            ));
        }
        let coverage = records
            .iter()
            .map(|record| {
                let summary = &record.manifest.summary;
                let asked = summary.ok_rows + summary.invalid_output_rows;
                let run = &record.manifest.run;
                RecordCoverage {
                    record_id: record.manifest.record_id.clone(),
                    model_id: record.manifest.lineage.model_id.clone(),
                    model_name: record.manifest.lineage.model_name.clone(),
                    rows: record.manifest.row_count,
                    ok_rows: summary.ok_rows,
                    skipped_rows: summary.skipped_rows,
                    invalid_output_rows: summary.invalid_output_rows,
                    coverage: ratio(summary.ok_rows, record.manifest.row_count).unwrap_or(0.0),
                    invalid_output_rate: ratio(summary.invalid_output_rows, asked),
                    mean_latency_us: (run.invoked_rows > 0)
                        .then(|| run.total_latency_us as f64 / run.invoked_rows as f64),
                    max_latency_us: run.max_latency_us,
                }
            })
            .collect();
        let mut pairs = Vec::new();
        for left in 0..records.len() {
            for right in left + 1..records.len() {
                pairs.push(compare_pair(&records[left], &records[right]));
            }
        }
        let input = &records[0].manifest.identity.input;
        Ok(ComparisonReport {
            input_kind: match input.kind {
                crate::spec::InputKind::FeatureSet => "FEATURE_SET".into(),
                crate::spec::InputKind::DecisionSet => "DECISION_SET".into(),
            },
            input_id: input.id.clone(),
            input_output_sha256: input.output_sha256.clone(),
            rows: rows as u64,
            records: coverage,
            pairs,
            notice: COMPARISON_NOTICE.into(),
        })
    }
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn load(
    manifest: InferenceRecordManifest,
    path: &std::path::Path,
    cancel: &AtomicBool,
) -> Result<LoadedRecord, ModelError> {
    let table = InputTable::new(
        manifest
            .columns
            .iter()
            .map(|column| (column.name.clone(), column_type(column.kind)))
            .collect(),
        manifest.row_count,
        &manifest.output_fingerprint,
    )
    .map_err(|reason| integrity(&manifest, reason))?;
    let status_column = table
        .position("row_status")
        .ok_or_else(|| integrity(&manifest, "record has no row_status column".into()))?;
    let output_columns = manifest
        .outputs
        .iter()
        .map(|output| {
            table
                .position(&format!("output__{}", output.name))
                .ok_or_else(|| {
                    integrity(&manifest, format!("record has no output__{}", output.name))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut decision_times = Vec::with_capacity(manifest.row_count as usize);
    let mut ok = Vec::with_capacity(manifest.row_count as usize);
    let mut values = vec![Vec::with_capacity(manifest.row_count as usize); manifest.outputs.len()];
    let read = input::read_rows(path, &table, |row| -> Result<(), ModelError> {
        if decision_times.len() % 1024 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        decision_times.push(row.time(table.decision_time_column));
        let row_ok = row.text(status_column) == Some("OK");
        ok.push(row_ok);
        for ((output, column), values) in manifest
            .outputs
            .iter()
            .zip(&output_columns)
            .zip(&mut values)
        {
            values.push(match (&output.kind, row_ok) {
                (_, false) => Value::Missing,
                (OutputKind::Float64 { .. }, true) => {
                    row.float(*column).map_or(Value::Missing, Value::Number)
                }
                (OutputKind::Category { .. }, true) => row
                    .text(*column)
                    .map_or(Value::Missing, |label| Value::Label(label.into())),
            });
        }
        Ok(())
    })
    .map_err(|reason| integrity(&manifest, reason))?;
    read?;
    Ok(LoadedRecord {
        manifest,
        decision_times,
        ok,
        values,
    })
}

fn integrity(manifest: &InferenceRecordManifest, reason: String) -> ModelError {
    ModelError::RecordNotUsable {
        record_id: manifest.record_id.clone(),
        status: format!("QUARANTINED ({reason})"),
    }
}

fn compare_pair(left: &LoadedRecord, right: &LoadedRecord) -> PairComparison {
    let mut fields = Vec::new();
    for (left_index, output) in left.manifest.outputs.iter().enumerate() {
        let Some(right_index) =
            right.manifest.outputs.iter().position(|other| {
                other.name == output.name && same_kind(&other.kind, &output.kind)
            })
        else {
            continue;
        };
        let pairs = left.values[left_index]
            .iter()
            .zip(&right.values[right_index])
            .zip(left.ok.iter().zip(&right.ok))
            .filter(|(_, (l, r))| **l && **r)
            .map(|(values, _)| values);
        let mut both = 0u64;
        let mut agreeing = 0u64;
        let mut difference_sum = 0.0f64;
        let mut difference_max: Option<f64> = None;
        for (l, r) in pairs {
            match (l, r) {
                (Value::Label(l), Value::Label(r)) => {
                    both += 1;
                    if l == r {
                        agreeing += 1;
                    }
                }
                (Value::Number(l), Value::Number(r)) => {
                    both += 1;
                    let difference = (l - r).abs();
                    difference_sum += difference;
                    difference_max =
                        Some(difference_max.map_or(difference, |max| max.max(difference)));
                }
                _ => {}
            }
        }
        let category = matches!(output.kind, OutputKind::Category { .. });
        fields.push(FieldAgreement {
            field: output.name.clone(),
            kind: if category { "CATEGORY" } else { "FLOAT64" }.into(),
            rows_both_ok: both,
            agreeing_rows: category.then_some(agreeing),
            agreement_rate: if category {
                ratio(agreeing, both)
            } else {
                None
            },
            mean_absolute_difference: (!category && both > 0).then(|| difference_sum / both as f64),
            max_absolute_difference: if category { None } else { difference_max },
        });
    }
    PairComparison {
        left_record_id: left.manifest.record_id.clone(),
        right_record_id: right.manifest.record_id.clone(),
        fields,
    }
}

fn same_kind(left: &OutputKind, right: &OutputKind) -> bool {
    matches!(
        (left, right),
        (OutputKind::Float64 { .. }, OutputKind::Float64 { .. })
            | (OutputKind::Category { .. }, OutputKind::Category { .. })
    )
}
