//! Synthetic, labeled fixtures for Model Registry integration tests.
//!
//! Bars come from closed-form price paths and pass through the real Data Ground importer, Feature
//! Studio, and Strategy Engine stores, so every inference reads registered, hash-verified research
//! inputs. Models are labeled synthetic ONNX fixtures or a loopback mock Ollama runtime. None of it
//! is market data or a trained model.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};

use chrono::{DateTime, Duration, Utc};
use rg_data_ground::{
    store::DatasetStore, CalendarPolicy, CsvDelimiter, FieldMapping, ImportRequest, MarketRole,
    ResourceQuotas, Timeframe, TimestampSemantics,
};
use rg_feature_studio::{
    store::FeatureStore, FeatureProgress, FeatureQuotas, FeatureSetRequest, FeatureSource,
    FeatureSpec, ParameterValue, SeriesSelector,
};
use rg_model_registry::{
    fixtures::ollama::MockOllama,
    store::{InferenceCommitResult, ModelLabStore, PreparedInference},
    InferenceProgress, InferenceQuotas, InferenceRequest, InputColumn, InputSetRef, ModelError,
    ModelRegistrationRequest, OutputField, OutputKind, RuntimeConfig,
};
use rg_strategy_engine::{
    store::StrategyStore, DecisionProgress, DecisionQuotas, DecisionSetRequest, FeatureBinding,
};
use tempfile::TempDir;

pub const FIXTURE_SOURCE_PREFIX: &str = "synthetic-fixture:";

#[derive(Clone, Debug)]
pub struct Bar {
    pub open_utc: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

pub fn bars(count: usize, phase: f64) -> Vec<Bar> {
    let step = Duration::hours(1);
    let mut time = DateTime::parse_from_rfc3339("2024-03-04T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut previous = 1.1 + 0.01 * phase.sin();
    (0..count)
        .map(|index| {
            let x = index as f64;
            let close = 1.1 + 0.012 * (x * 0.31 + phase).sin() + 0.003 * (x * 0.083).cos();
            let bar = Bar {
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
            "{},EURUSD.m,H1,{},{},{},{},1\n",
            bar.open_utc
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
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
    pub strategies: Arc<StrategyStore>,
    pub lab: Option<ModelLabStore>,
}

impl Fixture {
    pub fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("app-data");
        let datasets = Arc::new(DatasetStore::open(&root).unwrap());
        let features = Arc::new(FeatureStore::open(&root, Arc::clone(&datasets)).unwrap());
        let strategies = Arc::new(StrategyStore::open(&root, Arc::clone(&features)).unwrap());
        let lab = ModelLabStore::open(&root, Arc::clone(&strategies)).unwrap();
        Self {
            directory,
            root,
            datasets,
            features,
            strategies,
            lab: Some(lab),
        }
    }

    pub fn lab(&self) -> &ModelLabStore {
        self.lab.as_ref().unwrap()
    }

    /// Simulates an application restart of the model registry.
    pub fn reopen(&mut self) -> Result<(), ModelError> {
        self.lab = None;
        self.lab = Some(ModelLabStore::open(
            &self.root,
            Arc::clone(&self.strategies),
        )?);
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
            default_symbol: "EURUSD.m".into(),
            default_timeframe: Timeframe::H1.as_str().into(),
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

    /// Registers a feature set with `fast` SMA(3), `slow` SMA(8), and `roc` ROC(2) on the target.
    pub fn feature_set(&self, dataset_id: &str) -> String {
        let feature = |key: &str, indicator: &str, period: i64| FeatureSpec {
            key: key.into(),
            indicator_id: indicator.into(),
            indicator_version: 1,
            parameters: BTreeMap::from([("period".to_owned(), ParameterValue::Integer(period))]),
            source: FeatureSource::Target,
        };
        let request = FeatureSetRequest {
            target: SeriesSelector {
                dataset_id: dataset_id.into(),
                symbol: "EURUSD.m".into(),
                timeframe: "H1".into(),
            },
            features: vec![
                feature("fast", "SMA", 3),
                feature("slow", "SMA", 8),
                feature("roc", "ROC", 2),
            ],
            quotas: FeatureQuotas {
                max_target_rows: 1_000_000,
                max_input_rows: 2_000_000,
                max_output_bytes: 256 * 1024 * 1024,
            },
        };
        let prepared = self
            .features
            .prepare(
                &request,
                &AtomicBool::new(false),
                &FeatureProgress::default(),
            )
            .unwrap();
        self.features
            .commit_prepared(prepared)
            .unwrap()
            .feature_set_id
    }

    /// Registers a THRESHOLD_BAND decision set on `roc`.
    pub fn decision_set(&self, feature_set_id: &str) -> String {
        let request = DecisionSetRequest {
            feature_set_id: feature_set_id.into(),
            strategy_id: "THRESHOLD_BAND".into(),
            strategy_version: 1,
            parameters: BTreeMap::from([("upper".into(), 0.3), ("lower".into(), -0.3)]),
            bindings: BTreeMap::from([(
                "signal".into(),
                FeatureBinding {
                    feature_key: "roc".into(),
                    output: "value".into(),
                },
            )]),
            quotas: DecisionQuotas {
                max_rows: 1_000_000,
                max_output_bytes: 256 * 1024 * 1024,
            },
        };
        let prepared = self
            .strategies
            .prepare(
                &request,
                &AtomicBool::new(false),
                &DecisionProgress::default(),
            )
            .unwrap();
        self.strategies
            .commit_prepared(prepared)
            .unwrap()
            .decision_set_id
    }

    /// Imports the standard bars and registers a feature set over them.
    pub fn standard_feature_set(&self) -> String {
        let dataset = self.import("standard", &bars(60, 0.0));
        self.feature_set(&dataset)
    }

    pub fn write_file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.directory.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Stages, previews, and registers an ONNX fixture model; returns its model ID.
    pub fn register_onnx(
        &self,
        file_name: &str,
        bytes: &[u8],
        input_fields: &[&str],
        outputs: Vec<OutputField>,
    ) -> String {
        let path = self.write_file(file_name, bytes);
        let candidate = self
            .lab()
            .stage_onnx_candidate(&path, &AtomicBool::new(false))
            .unwrap();
        assert!(candidate.eligible, "{:?}", candidate.reason);
        let request = onnx_registration(&candidate.candidate_id, input_fields, outputs);
        let preview = self.lab().preview_registration(&request).unwrap();
        self.lab()
            .register_model(&request, &preview.model_id)
            .unwrap()
            .model_id
    }

    pub fn register_ollama(
        &self,
        mock: &MockOllama,
        seed: u32,
        outputs: Vec<OutputField>,
    ) -> String {
        self.lab()
            .set_ollama_endpoint(&mock.endpoint().to_string())
            .unwrap();
        let request = ollama_registration(seed, outputs);
        let preview = self.lab().preview_registration(&request).unwrap();
        self.lab()
            .register_model(&request, &preview.model_id)
            .unwrap()
            .model_id
    }

    pub fn prepare(&self, request: &InferenceRequest) -> Result<PreparedInference, ModelError> {
        self.lab().prepare_inference(
            request,
            &AtomicBool::new(false),
            &InferenceProgress::default(),
        )
    }

    pub fn infer(&self, request: &InferenceRequest) -> InferenceCommitResult {
        let prepared = self.prepare(request).unwrap();
        self.lab().commit_inference(prepared).unwrap()
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

    pub fn model_dir(&self, model_id: &str) -> PathBuf {
        self.root.join("models").join(model_id)
    }

    pub fn record_dir(&self, record_id: &str) -> PathBuf {
        self.root.join("inference-records").join(record_id)
    }

    pub fn staging_entries(&self) -> usize {
        std::fs::read_dir(self.root.join(".model-lab-staging").join("runs"))
            .unwrap()
            .count()
    }

    pub fn registry_rows(&self, table: &str) -> u32 {
        let connection = rusqlite::Connection::open(self.root.join("research.sqlite3")).unwrap();
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

pub fn float_output(name: &str, minimum: f64, maximum: f64) -> OutputField {
    OutputField {
        name: name.into(),
        kind: OutputKind::Float64 { minimum, maximum },
    }
}

pub fn category_output(name: &str, labels: &[&str]) -> OutputField {
    OutputField {
        name: name.into(),
        kind: OutputKind::Category {
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
        },
    }
}

pub fn onnx_registration(
    candidate_id: &str,
    input_fields: &[&str],
    outputs: Vec<OutputField>,
) -> ModelRegistrationRequest {
    ModelRegistrationRequest {
        candidate_id: candidate_id.into(),
        input_fields: input_fields
            .iter()
            .map(|field| (*field).to_owned())
            .collect(),
        outputs,
        runtime: RuntimeConfig::OnnxNative {},
        authority: "ADVISORY_ONLY".into(),
    }
}

pub fn ollama_registration(seed: u32, outputs: Vec<OutputField>) -> ModelRegistrationRequest {
    use rg_model_registry::fixtures::ollama::{FIXTURE_DIGEST, FIXTURE_MODEL};
    ModelRegistrationRequest {
        candidate_id: format!("ollama:{FIXTURE_MODEL}@{FIXTURE_DIGEST}"),
        input_fields: vec!["fast".into(), "slow".into()],
        outputs,
        runtime: RuntimeConfig::OllamaLoopback {
            seed,
            max_output_tokens: 64,
            context_tokens: 2048,
            instruction: "Classify the direction of the synthetic fixture inputs.".into(),
        },
        authority: "ADVISORY_ONLY".into(),
    }
}

pub fn feature(key: &str) -> InputColumn {
    InputColumn::Feature {
        feature_key: key.into(),
        output: "value".into(),
    }
}

pub fn quotas() -> InferenceQuotas {
    InferenceQuotas {
        max_rows: 1_000_000,
        max_output_bytes: 64 * 1024 * 1024,
        request_timeout_ms: 5_000,
    }
}

pub fn feature_request(
    model_id: &str,
    feature_set_id: &str,
    bindings: &[(&str, InputColumn)],
) -> InferenceRequest {
    InferenceRequest {
        model_id: model_id.into(),
        input: InputSetRef::FeatureSet {
            feature_set_id: feature_set_id.into(),
        },
        bindings: bindings
            .iter()
            .map(|(field, column)| ((*field).to_owned(), column.clone()))
            .collect(),
        authority: "ADVISORY_ONLY".into(),
        quotas: quotas(),
    }
}

pub fn decision_request(
    model_id: &str,
    decision_set_id: &str,
    bindings: &[(&str, InputColumn)],
) -> InferenceRequest {
    InferenceRequest {
        input: InputSetRef::DecisionSet {
            decision_set_id: decision_set_id.into(),
        },
        ..feature_request(model_id, "fs_unused", bindings)
    }
}

/// Every file below `directory`, with its bytes, for byte-identity assertions.
pub fn snapshot(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    files
}

/// Reads the `output__<field>` float column of an inference output in row order.
pub fn output_values(path: &Path, field: &str) -> Vec<Option<f64>> {
    float_column(path, &format!("output__{field}"))
}

/// Reads a nullable float column of a Parquet file in row order.
pub fn float_column(path: &Path, column_name: &str) -> Vec<Option<f64>> {
    use arrow::array::{Array, Float64Array};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let mut values = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let column = batch
            .column_by_name(column_name)
            .unwrap()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .clone();
        for row in 0..column.len() {
            values.push((!column.is_null(row)).then(|| column.value(row)));
        }
    }
    values
}

pub fn text_values(path: &Path, column_name: &str) -> Vec<Option<String>> {
    use arrow::array::{Array, StringArray};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path).unwrap())
        .unwrap()
        .build()
        .unwrap();
    let mut values = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let column = batch
            .column_by_name(column_name)
            .unwrap()
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap()
            .clone();
        for row in 0..column.len() {
            values.push((!column.is_null(row)).then(|| column.value(row).to_owned()));
        }
    }
    values
}
