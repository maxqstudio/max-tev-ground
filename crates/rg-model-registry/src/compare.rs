//! Descriptive comparison of inference records computed over the same input identity.
//!
//! Reports coverage, invalid-output rates, agreement between records, absolute differences
//! between their numeric outputs, and recorded latencies. It has no labels, prices, or positions,
//! so it reports no accuracy against outcomes, PnL, win rate, profitability, or edge.
//!
//! Resources are bounded in aggregate before anything is copied or allocated: the selected
//! records' rows times (outputs + 1) must fit [`ModelLabLimits::max_comparison_cells`], and their
//! output files together must fit [`ModelLabLimits::max_comparison_copy_bytes`]. Each loaded cell
//! takes at most 8 bytes (a number, or a one-byte label index), and only one private copy exists
//! at a time.

use std::{
    collections::BTreeSet,
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    inference::copy_hashed,
    input::{self, ColumnType, InputTable},
    spec::{InferenceColumnKind, InferenceRecordManifest, OutputKind},
    store::{ModelLabLimits, ModelLabStore},
    ModelError,
};

pub const MAX_COMPARED_RECORDS: usize = 8;
/// Upper bound on bytes held per loaded cell: an `f64`, or a one-byte label index.
pub const BYTES_PER_COMPARISON_CELL: u64 = 8;
const MISSING_LABEL: u8 = u8::MAX;

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

/// One output column held compactly: numbers with `NaN` for rows without an OK value (OK values
/// are always finite), or label indexes into the declared labels with [`MISSING_LABEL`].
enum StoredColumn {
    Numbers(Vec<f64>),
    Labels(Vec<u8>),
}

struct LoadedRecord {
    manifest: InferenceRecordManifest,
    /// SHA-256 over the record's decision times in row order.
    decision_times_sha256: Vec<u8>,
    rows: u64,
    /// Indexed by declared output.
    values: Vec<StoredColumn>,
}

/// Cells a comparison would hold: for each record, rows times (outputs + 1). Refuses an
/// overflowing or over-budget request before anything is copied or allocated.
pub fn comparison_cells(
    records: &[(u64, usize)],
    limits: &ModelLabLimits,
) -> Result<u64, ModelError> {
    let mut total = 0u64;
    for (rows, outputs) in records {
        let cells = (*outputs as u64)
            .checked_add(1)
            .and_then(|columns| rows.checked_mul(columns))
            .and_then(|cells| total.checked_add(cells));
        total = match cells {
            Some(total) if total <= limits.max_comparison_cells => total,
            _ => {
                return Err(ModelError::ResourceQuota(format!(
                    "comparing these records needs more than {} cells (records x rows x (outputs + 1)); select fewer records or outputs",
                    limits.max_comparison_cells
                )))
            }
        };
    }
    Ok(total)
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
        // Verify every record and check the aggregate budget before copying or allocating.
        let limits = self.limits();
        let mut selected: Vec<(InferenceRecordManifest, std::path::PathBuf)> =
            Vec::with_capacity(request.record_ids.len());
        let mut copy_bytes = 0u64;
        for record_id in &request.record_ids {
            let (manifest, path) = self.usable_record(record_id)?;
            if let Some((first, _)) = selected.first() {
                if first.identity.input != manifest.identity.input {
                    return Err(ModelError::ComparisonRefused(format!(
                        "{record_id} was computed over a different input identity than {}",
                        first.record_id
                    )));
                }
            }
            copy_bytes = copy_bytes
                .checked_add(fs::metadata(&path)?.len())
                .filter(|bytes| *bytes <= limits.max_comparison_copy_bytes)
                .ok_or_else(|| {
                    ModelError::ResourceQuota(format!(
                        "the selected record outputs exceed the {} byte comparison copy budget",
                        limits.max_comparison_copy_bytes
                    ))
                })?;
            selected.push((manifest, path));
        }
        comparison_cells(
            &selected
                .iter()
                .map(|(manifest, _)| (manifest.row_count, manifest.outputs.len()))
                .collect::<Vec<_>>(),
            &limits,
        )?;
        let mut records: Vec<LoadedRecord> = Vec::with_capacity(selected.len());
        let scratch = self.staging_root().join(Uuid::now_v7().to_string());
        fs::create_dir(&scratch)?;
        let result = (|| {
            for (index, (manifest, path)) in selected.into_iter().enumerate() {
                let copy = scratch.join(format!("{index}.parquet"));
                let (digest, _) =
                    copy_hashed(&path, &copy, limits.max_comparison_copy_bytes, cancel)?;
                if digest != manifest.output_sha256 {
                    return Err(ModelError::RecordNotUsable {
                        record_id: manifest.record_id.clone(),
                        status: "QUARANTINED".into(),
                    });
                }
                let loaded = load(manifest, &copy, cancel)?;
                // Only one private copy exists at a time.
                fs::remove_file(&copy)?;
                records.push(loaded);
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&scratch);
        result?;
        let rows = records[0].rows;
        if records.iter().any(|record| {
            record.rows != rows || record.decision_times_sha256 != records[0].decision_times_sha256
        }) {
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
            rows,
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
    let rows = usize::try_from(manifest.row_count)
        .map_err(|_| ModelError::ResourceQuota("record is too large to compare".into()))?;
    let mut times = Sha256::new();
    let mut count = 0u64;
    let mut values = manifest
        .outputs
        .iter()
        .map(|output| match output.kind {
            OutputKind::Float64 { .. } => StoredColumn::Numbers(Vec::with_capacity(rows)),
            OutputKind::Category { .. } => StoredColumn::Labels(Vec::with_capacity(rows)),
        })
        .collect::<Vec<_>>();
    let read = input::read_rows(path, &table, |row| -> Result<(), ModelError> {
        if count.is_multiple_of(1024) && cancel.load(Ordering::Relaxed) {
            return Err(ModelError::Cancelled);
        }
        count += 1;
        times.update(row.time(table.decision_time_column).to_be_bytes());
        let row_ok = row.text(status_column) == Some("OK");
        for ((output, column), stored) in manifest
            .outputs
            .iter()
            .zip(&output_columns)
            .zip(&mut values)
        {
            match (stored, &output.kind) {
                (StoredColumn::Numbers(numbers), _) => numbers.push(
                    row.float(*column)
                        .filter(|value| row_ok && value.is_finite())
                        .unwrap_or(f64::NAN),
                ),
                (StoredColumn::Labels(labels), OutputKind::Category { labels: declared }) => {
                    let index = row
                        .text(*column)
                        .filter(|_| row_ok)
                        .and_then(|label| declared.iter().position(|known| known == label))
                        .map_or(MISSING_LABEL, |index| index as u8);
                    labels.push(index);
                }
                (StoredColumn::Labels(labels), _) => labels.push(MISSING_LABEL),
            }
        }
        Ok(())
    })
    .map_err(|reason| integrity(&manifest, reason))?;
    read?;
    Ok(LoadedRecord {
        manifest,
        decision_times_sha256: times.finalize().to_vec(),
        rows: count,
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
        let right_output = &right.manifest.outputs[right_index];
        let mut both = 0u64;
        let mut agreeing = 0u64;
        let mut difference_sum = 0.0f64;
        let mut difference_max: Option<f64> = None;
        match (&left.values[left_index], &right.values[right_index]) {
            (StoredColumn::Numbers(l), StoredColumn::Numbers(r)) => {
                for (l, r) in l.iter().zip(r) {
                    if l.is_nan() || r.is_nan() {
                        continue;
                    }
                    both += 1;
                    let difference = (l - r).abs();
                    difference_sum += difference;
                    difference_max =
                        Some(difference_max.map_or(difference, |max| max.max(difference)));
                }
            }
            (StoredColumn::Labels(l), StoredColumn::Labels(r)) => {
                let (
                    OutputKind::Category {
                        labels: left_labels,
                    },
                    OutputKind::Category {
                        labels: right_labels,
                    },
                ) = (&output.kind, &right_output.kind)
                else {
                    continue;
                };
                for (l, r) in l.iter().zip(r) {
                    if *l == MISSING_LABEL || *r == MISSING_LABEL {
                        continue;
                    }
                    both += 1;
                    if left_labels[*l as usize] == right_labels[*r as usize] {
                        agreeing += 1;
                    }
                }
            }
            _ => continue,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DEFAULT_LIMITS;

    #[test]
    fn gate_model_comparison_aggregate_budget_scales_with_records_rows_and_outputs() {
        let limits = ModelLabLimits {
            max_comparison_cells: 1_000,
            ..DEFAULT_LIMITS
        };
        // 2 records x 100 rows x (4 outputs + 1) = 1000 cells: exactly at the budget.
        assert_eq!(
            comparison_cells(&[(100, 4), (100, 4)], &limits).unwrap(),
            1_000
        );
        // One more record, one more row, or one more output each exceeds it.
        for request in [
            vec![(100, 4), (100, 4), (1, 0)],
            vec![(101, 4), (100, 4)],
            vec![(100, 5), (100, 4)],
        ] {
            assert!(matches!(
                comparison_cells(&request, &limits),
                Err(ModelError::ResourceQuota(_))
            ));
        }
    }

    #[test]
    fn gate_resource_cancel_recovery_comparison_budget_refuses_large_and_overflowing_requests() {
        // The largest request the record limits allow: 8 records x 5,000,000 rows x 16 outputs.
        let largest = vec![(5_000_000u64, 16usize); MAX_COMPARED_RECORDS];
        assert!(matches!(
            comparison_cells(&largest, &DEFAULT_LIMITS),
            Err(ModelError::ResourceQuota(_))
        ));
        assert!(matches!(
            comparison_cells(&[(u64::MAX, 16), (u64::MAX, 16)], &DEFAULT_LIMITS),
            Err(ModelError::ResourceQuota(_))
        ));
        assert!(matches!(
            comparison_cells(
                &[(u64::MAX / 2, 0), (u64::MAX / 2, 0), (2, 0)],
                &DEFAULT_LIMITS
            ),
            Err(ModelError::ResourceQuota(_))
        ));
        // The default budget bounds resident comparison data to at most 256 MiB.
        const {
            assert!(
                DEFAULT_LIMITS.max_comparison_cells * BYTES_PER_COMPARISON_CELL
                    <= 256 * 1024 * 1024
            )
        };
    }
}
