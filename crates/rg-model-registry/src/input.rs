//! Streaming reader for a verified RG-R02 feature-set or RG-R03 decision-set output.
//!
//! The reader works on a private staged copy whose SHA-256 already matched the registered manifest.
//! It checks the Parquet schema against the manifest's declared columns and recomputes the
//! upstream row fingerprint from the exact rows it hands to the model, so a run fails closed
//! unless every value it used is the registered content.

use std::{fs::File, path::Path};

use arrow::{
    array::{Array, Float64Array, RecordBatch, StringArray, TimestampMicrosecondArray},
    datatypes::{DataType, TimeUnit},
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rg_feature_studio::{spec::ColumnKind, FeatureSetManifest};
use rg_strategy_engine::{spec::DecisionColumnKind, DecisionSetManifest};
use sha2::{Digest, Sha256};

use crate::spec::hex;

const READ_BATCH_ROWS: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnType {
    Timestamp,
    NullableTimestamp,
    Float,
    Text,
    NullableText,
}

impl ColumnType {
    fn data_type(self) -> DataType {
        match self {
            Self::Timestamp | Self::NullableTimestamp => {
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
            }
            Self::Float => DataType::Float64,
            Self::Text | Self::NullableText => DataType::Utf8,
        }
    }

    fn nullable(self) -> bool {
        matches!(
            self,
            Self::NullableTimestamp | Self::Float | Self::NullableText
        )
    }
}

/// The declared layout and registered fingerprint of one input table.
#[derive(Clone, Debug)]
pub(crate) struct InputTable {
    pub columns: Vec<(String, ColumnType)>,
    pub row_count: u64,
    pub fingerprint: String,
    pub decision_time_column: usize,
    pub open_time_column: usize,
}

impl InputTable {
    pub fn position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(column, _)| column == name)
    }

    pub fn for_feature_set(manifest: &FeatureSetManifest) -> Result<Self, String> {
        let columns = manifest
            .columns
            .iter()
            .map(|column| {
                let kind = match column.kind {
                    ColumnKind::TimestampUtc => ColumnType::Timestamp,
                    ColumnKind::NullableTimestampUtc => ColumnType::NullableTimestamp,
                    ColumnKind::NullableFloat64 => ColumnType::Float,
                    ColumnKind::Status => ColumnType::Text,
                };
                (column.name.clone(), kind)
            })
            .collect();
        Self::new(columns, manifest.row_count, &manifest.output_fingerprint)
    }

    pub fn for_decision_set(manifest: &DecisionSetManifest) -> Result<Self, String> {
        let columns = manifest
            .columns
            .iter()
            .map(|column| {
                let kind = match column.kind {
                    DecisionColumnKind::Text => ColumnType::Text,
                    DecisionColumnKind::NullableText => ColumnType::NullableText,
                    DecisionColumnKind::TimestampUtc => ColumnType::Timestamp,
                    DecisionColumnKind::NullableFloat64 => ColumnType::Float,
                };
                (column.name.clone(), kind)
            })
            .collect();
        Self::new(columns, manifest.row_count, &manifest.output_fingerprint)
    }

    pub fn new(
        columns: Vec<(String, ColumnType)>,
        row_count: u64,
        fingerprint: &str,
    ) -> Result<Self, String> {
        let find = |name: &str| {
            columns
                .iter()
                .position(|(column, kind)| column == name && *kind == ColumnType::Timestamp)
                .ok_or_else(|| format!("input declares no {name} column"))
        };
        Ok(Self {
            decision_time_column: find("decision_time_utc")?,
            open_time_column: find("open_time_utc")?,
            columns,
            row_count,
            fingerprint: fingerprint.into(),
        })
    }
}

enum Column {
    Times(TimestampMicrosecondArray),
    Floats(Float64Array),
    Texts(StringArray),
}

/// Read-only view of one input row.
pub(crate) struct Row<'a> {
    columns: &'a [Column],
    row: usize,
}

impl Row<'_> {
    pub fn time(&self, column: usize) -> i64 {
        match &self.columns[column] {
            Column::Times(values) => values.value(self.row),
            _ => unreachable!("planned time columns are timestamp columns"),
        }
    }

    pub fn float(&self, column: usize) -> Option<f64> {
        match &self.columns[column] {
            Column::Floats(values) => (!values.is_null(self.row)).then(|| values.value(self.row)),
            _ => unreachable!("planned value columns are float columns"),
        }
    }

    pub fn text(&self, column: usize) -> Option<&str> {
        match &self.columns[column] {
            Column::Texts(values) => (!values.is_null(self.row)).then(|| values.value(self.row)),
            _ => unreachable!("planned text columns are text columns"),
        }
    }
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Streams every row of `path` to `visit`, in file order. Returns an integrity diagnostic if the
/// file's schema, row order, row count, or recomputed fingerprint differs from `table`.
pub(crate) fn read_rows<E>(
    path: &Path,
    table: &InputTable,
    mut visit: impl FnMut(&Row<'_>) -> Result<(), E>,
) -> Result<Result<(), E>, String> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(
        File::open(path).map_err(|error| format!("input copy cannot be opened: {error}"))?,
    )
    .map_err(|error| format!("input is not readable Parquet: {error}"))?;
    let schema = builder.schema();
    let schema_matches = schema.fields().len() == table.columns.len()
        && schema
            .fields()
            .iter()
            .zip(&table.columns)
            .all(|(field, (name, kind))| {
                field.name() == name
                    && *field.data_type() == kind.data_type()
                    && field.is_nullable() == kind.nullable()
            });
    if !schema_matches {
        return Err("input schema does not match its registered manifest".into());
    }
    let reader = builder
        .with_batch_size(READ_BATCH_ROWS)
        .build()
        .map_err(|error| format!("input reader failed: {error}"))?;
    let mut fingerprint = Sha256::new();
    let mut rows = 0u64;
    let mut last_decision: Option<i64> = None;
    for batch in reader {
        let batch: RecordBatch = batch.map_err(|error| format!("input batch failed: {error}"))?;
        let columns = table
            .columns
            .iter()
            .enumerate()
            .map(|(index, (name, kind))| {
                let array = batch.column(index).as_any();
                let column = match kind {
                    ColumnType::Timestamp | ColumnType::NullableTimestamp => array
                        .downcast_ref::<TimestampMicrosecondArray>()
                        .cloned()
                        .map(Column::Times),
                    ColumnType::Float => array
                        .downcast_ref::<Float64Array>()
                        .cloned()
                        .map(Column::Floats),
                    ColumnType::Text | ColumnType::NullableText => array
                        .downcast_ref::<StringArray>()
                        .cloned()
                        .map(Column::Texts),
                };
                column.ok_or_else(|| format!("column {name} has an unexpected type"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            // The upstream fingerprint, recomputed over every column in manifest order.
            for (column, (name, kind)) in columns.iter().zip(&table.columns) {
                match (column, kind) {
                    (Column::Times(values), ColumnType::Timestamp) => {
                        if values.is_null(row) {
                            return Err(format!("{name} contains a null time"));
                        }
                        fingerprint.update(values.value(row).to_be_bytes());
                    }
                    (Column::Times(values), _) => {
                        if values.is_null(row) {
                            fingerprint.update([0]);
                        } else {
                            fingerprint.update([1]);
                            fingerprint.update(values.value(row).to_be_bytes());
                        }
                    }
                    (Column::Floats(values), _) => {
                        if values.is_null(row) {
                            fingerprint.update([0]);
                        } else {
                            fingerprint.update([1]);
                            fingerprint.update(values.value(row).to_bits().to_be_bytes());
                        }
                    }
                    (Column::Texts(values), ColumnType::Text) => {
                        if values.is_null(row) {
                            return Err(format!("{name} contains a null value"));
                        }
                        hash_field(&mut fingerprint, values.value(row).as_bytes());
                    }
                    (Column::Texts(values), _) => {
                        if values.is_null(row) {
                            fingerprint.update([0]);
                        } else {
                            fingerprint.update([1]);
                            hash_field(&mut fingerprint, values.value(row).as_bytes());
                        }
                    }
                }
            }
            let view = Row {
                columns: &columns,
                row,
            };
            let decision = view.time(table.decision_time_column);
            if last_decision.is_some_and(|last| decision <= last) {
                return Err("input rows are not in strictly increasing decision-time order".into());
            }
            last_decision = Some(decision);
            rows += 1;
            if rows > table.row_count {
                return Err("input has more rows than its manifest declares".into());
            }
            if let Err(error) = visit(&view) {
                return Ok(Err(error));
            }
        }
    }
    if rows != table.row_count || hex(fingerprint.finalize()) != table.fingerprint {
        return Err("rows read do not reproduce the registered row count and fingerprint".into());
    }
    Ok(Ok(()))
}
