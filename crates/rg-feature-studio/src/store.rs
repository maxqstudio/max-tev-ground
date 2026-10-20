//! Immutable feature-set store: staged materialization, journaled promotion, crash recovery, and
//! verification of every catalogue read.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use rg_data_ground::{store::DatasetStore, DatasetStatus, SeriesSummary};
use rg_persistence::{
    clear_pending_feature_set_promotion, complete_pending_feature_set_promotion,
    get_dataset_metadata, get_feature_set_metadata, list_feature_attempts, list_feature_set_inputs,
    list_feature_set_metadata_after, list_pending_feature_set_promotions, open_database,
    save_feature_attempt, save_pending_feature_set_promotion, FeatureAttemptRecord,
    FeatureSetInput, FeatureSetMetadata, PendingFeatureSetPromotion,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    engine::{self, FeatureProgress, PreviewRow, ResolvedInput},
    spec::{
        hex, is_sha256, validate_request, FeatureSetIdentity, FeatureSetManifest,
        FeatureSetRequest, FeatureSetStatus, FeatureSource, InputDatasetIdentity, InputLineage,
        TemporalSemantics, ValidatedRequest, FEATURE_ENGINE_ID, FEATURE_ENGINE_VERSION,
        FEATURE_SET_SCHEMA_VERSION, RESEARCH_NOTICE,
    },
    FeatureError,
};

const CATALOG_PAGE_SIZE: u64 = 32;
const ATTEMPT_HISTORY_LIMIT: u64 = 50;
const OUTPUT_FILE: &str = "features.parquet";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug)]
pub struct PreparedFeatureSet {
    pub manifest: FeatureSetManifest,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub preview: FeatureSetPreview,
    pub staging_directory: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct FeatureSetPreview {
    pub feature_set_id: String,
    pub manifest_sha256: String,
    pub manifest: FeatureSetManifest,
    pub sample_rows: Vec<PreviewRow>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FeatureCommitDisposition {
    Materialized,
    Reused,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FeatureCommitResult {
    pub feature_set_id: String,
    pub status: FeatureSetStatus,
    pub disposition: FeatureCommitDisposition,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FeatureSetIntegrity {
    pub manifest_valid: bool,
    pub output_valid: bool,
    pub lineage_valid: bool,
}

impl FeatureSetIntegrity {
    fn is_valid(&self) -> bool {
        self.manifest_valid && self.output_valid && self.lineage_valid
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FeatureSetCatalogEntry {
    pub feature_set_id: String,
    pub identity_sha256: String,
    pub manifest_sha256: String,
    pub status: FeatureSetStatus,
    pub manifest: Option<FeatureSetManifest>,
    pub integrity: FeatureSetIntegrity,
    pub diagnostic: Option<String>,
}

/// A registered dataset as offered to Feature Studio. Only `eligible` entries may be selected.
#[derive(Clone, Debug, Serialize)]
pub struct FeatureInputCandidate {
    pub dataset_id: String,
    pub status: DatasetStatus,
    pub eligible: bool,
    pub reason: Option<String>,
    pub source_file_name: Option<String>,
    pub source_id: Option<String>,
    pub broker: Option<String>,
    pub server: Option<String>,
    pub created_at_utc: Option<String>,
    pub series: Vec<SeriesSummary>,
}

pub struct FeatureStore {
    datasets: Arc<DatasetStore>,
    staging_directory: PathBuf,
    feature_sets_directory: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    operation_lock: Mutex<()>,
}

impl FeatureStore {
    /// Opens the feature registry beside an already opened dataset store and finishes any
    /// interrupted promotion. Recovery fails closed and preserves evidence on any mismatch.
    pub fn open(root: impl AsRef<Path>, datasets: Arc<DatasetStore>) -> Result<Self, FeatureError> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;
        let staging_directory = ensure_store_directory(&root, ".feature-staging")?;
        let feature_sets_directory = ensure_store_directory(&root, "feature-sets")?;
        let database_path = root.join("research.sqlite3");
        match fs::symlink_metadata(&database_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            _ => return Err(FeatureError::UnsafeStorePath),
        }
        let store = Self {
            datasets,
            staging_directory,
            feature_sets_directory,
            connection: Mutex::new(open_database(database_path)?),
            operation_lock: Mutex::new(()),
        };
        store.recover_pending_promotions()?;
        Ok(store)
    }

    pub fn datasets(&self) -> &Arc<DatasetStore> {
        &self.datasets
    }

    /// Lists registered datasets with their verified status. Non-READY or unverifiable datasets
    /// are returned as ineligible so the UI can explain, not hide, why they cannot be used.
    pub fn list_input_candidates(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<FeatureInputCandidate>, FeatureError> {
        let entries = self
            .datasets
            .list_datasets_with_progress(cancel, |_, _, _| {})?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                let verified = entry.integrity.manifest_valid
                    && entry.integrity.raw_source_valid
                    && entry.integrity.canonical_parquet_valid;
                let reason = if !verified || entry.manifest.is_none() {
                    Some("Snapshot failed integrity verification".to_owned())
                } else if entry.status != DatasetStatus::Ready {
                    Some(format!(
                        "Dataset is {}; only READY datasets are trusted feature inputs",
                        entry.status.as_str()
                    ))
                } else {
                    None
                };
                let manifest = entry.manifest.as_ref();
                FeatureInputCandidate {
                    dataset_id: entry.dataset_id.clone(),
                    status: entry.status.clone(),
                    eligible: reason.is_none(),
                    reason,
                    source_file_name: manifest.map(|m| m.provenance.source_file_name.clone()),
                    source_id: manifest.map(|m| m.provenance.source_id.clone()),
                    broker: manifest.map(|m| m.provenance.broker.clone()),
                    server: manifest.map(|m| m.provenance.server.clone()),
                    created_at_utc: manifest.map(|m| m.created_at_utc.clone()),
                    series: manifest.map(|m| m.series.clone()).unwrap_or_default(),
                }
            })
            .collect())
    }

    /// Validates the request, verifies every input, and materializes the feature set into a
    /// private staging directory. Nothing is registered until [`FeatureStore::commit_prepared`].
    pub fn prepare(
        &self,
        request: &FeatureSetRequest,
        cancel: &AtomicBool,
        progress: &FeatureProgress,
    ) -> Result<PreparedFeatureSet, FeatureError> {
        let validated = validate_request(request)?;
        let inputs = self.resolve_inputs(&validated)?;
        let stage = self.staging_directory.join(Uuid::now_v7().to_string());
        fs::create_dir(&stage)?;
        let result = prepare_in_stage(&validated, &inputs, &stage, cancel, progress);
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    fn resolve_inputs(
        &self,
        validated: &ValidatedRequest,
    ) -> Result<BTreeMap<String, ResolvedInput>, FeatureError> {
        let mut inputs = BTreeMap::new();
        for dataset_id in &validated.dataset_ids {
            let entry = self
                .datasets
                .inspect_dataset(dataset_id)?
                .ok_or_else(|| FeatureError::InputMissing(dataset_id.clone()))?;
            let verified = entry.integrity.manifest_valid
                && entry.integrity.raw_source_valid
                && entry.integrity.canonical_parquet_valid;
            if !verified {
                return Err(FeatureError::InputIntegrity {
                    dataset_id: dataset_id.clone(),
                    reason: entry
                        .diagnostic
                        .unwrap_or_else(|| "snapshot digest verification failed".into()),
                });
            }
            if entry.status != DatasetStatus::Ready {
                return Err(FeatureError::InputNotReady {
                    dataset_id: dataset_id.clone(),
                    status: entry.status.as_str().into(),
                });
            }
            let manifest = entry.manifest.ok_or_else(|| FeatureError::InputIntegrity {
                dataset_id: dataset_id.clone(),
                reason: "verified manifest is unavailable".into(),
            })?;
            if manifest.bar_count > validated.quotas.max_input_rows {
                return Err(FeatureError::ResourceQuota(format!(
                    "dataset {dataset_id} has {} bars; the input-row quota is {}",
                    manifest.bar_count, validated.quotas.max_input_rows
                )));
            }
            let bars_path = self.datasets.canonical_bars_path(dataset_id)?;
            inputs.insert(
                dataset_id.clone(),
                ResolvedInput {
                    manifest,
                    bars_path,
                },
            );
        }
        let series_rows = |selector: &crate::spec::SeriesSelector| {
            inputs[&selector.dataset_id]
                .manifest
                .series
                .iter()
                .find(|series| {
                    series.symbol == selector.symbol && series.timeframe == selector.timeframe
                })
                .map(|series| series.bar_count)
                .ok_or_else(|| FeatureError::SeriesNotFound {
                    dataset_id: selector.dataset_id.clone(),
                    symbol: selector.symbol.clone(),
                    timeframe: selector.timeframe.clone(),
                })
        };
        let target_rows = series_rows(&validated.target)?;
        if target_rows > validated.quotas.max_target_rows {
            return Err(FeatureError::ResourceQuota(format!(
                "target series has {target_rows} bars; the target-row quota is {}",
                validated.quotas.max_target_rows
            )));
        }
        for (feature, _) in &validated.features {
            if let FeatureSource::Reference { series, .. } = &feature.source {
                series_rows(series)?;
            }
        }
        Ok(inputs)
    }

    pub fn commit_prepared(
        &self,
        prepared: PreparedFeatureSet,
    ) -> Result<FeatureCommitResult, FeatureError> {
        let _operation = self.operation_guard()?;
        let feature_set_id = prepared.manifest.feature_set_id.clone();
        let result = self.commit_locked(&prepared);
        let pending = self.has_pending_promotion(&feature_set_id).unwrap_or(true);
        if let Err(error) = &result {
            if !pending {
                let status = if matches!(error, FeatureError::Cancelled) {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self.save_attempt(
                    status,
                    Some(&feature_set_id),
                    Some(error.code()),
                    Some(&error.to_string()),
                );
            }
        }
        if !pending && prepared.staging_directory.exists() {
            let _ = fs::remove_dir_all(&prepared.staging_directory);
        }
        result
    }

    fn commit_locked(
        &self,
        prepared: &PreparedFeatureSet,
    ) -> Result<FeatureCommitResult, FeatureError> {
        let stage = verify_prepared(prepared, &self.staging_directory)?;
        let manifest = &prepared.manifest;
        for input in &manifest.identity.inputs {
            let entry = self
                .datasets
                .inspect_dataset(&input.dataset_id)?
                .ok_or_else(|| FeatureError::InputMissing(input.dataset_id.clone()))?;
            let unchanged = entry.status == DatasetStatus::Ready
                && entry.integrity.manifest_valid
                && entry.integrity.raw_source_valid
                && entry.integrity.canonical_parquet_valid
                && entry.manifest.as_ref().is_some_and(|dataset| {
                    dataset.fingerprint == input.fingerprint
                        && dataset.canonical_bars_sha256 == input.canonical_bars_sha256
                });
            if !unchanged {
                return Err(FeatureError::InputIntegrity {
                    dataset_id: input.dataset_id.clone(),
                    reason: "input changed or lost READY verification after preview".into(),
                });
            }
        }
        let existing = {
            let connection = self.connection_guard()?;
            get_feature_set_metadata(&connection, &manifest.feature_set_id)?
        };
        if let Some(existing) = existing {
            let entry = self.verify_metadata(&existing, &AtomicBool::new(false))?;
            if entry.status == FeatureSetStatus::Quarantined {
                return Err(FeatureError::ExistingFeatureSetCorrupt(
                    manifest.feature_set_id.clone(),
                ));
            }
            if existing.output_sha256 != manifest.output_sha256 {
                return Err(FeatureError::NonDeterministicReplay(
                    manifest.feature_set_id.clone(),
                ));
            }
            self.save_attempt(
                "REUSED",
                Some(&manifest.feature_set_id),
                Some("IDENTICAL_IDENTITY"),
                Some("Existing feature set reused after a bit-identical replay"),
            )?;
            let _ = fs::remove_dir_all(&stage);
            return Ok(FeatureCommitResult {
                feature_set_id: manifest.feature_set_id.clone(),
                status: entry.status,
                disposition: FeatureCommitDisposition::Reused,
            });
        }
        let destination = self.feature_sets_directory.join(&manifest.feature_set_id);
        if path_exists(&destination)? {
            return Err(FeatureError::OrphanDirectoryExists);
        }
        let staging_name = stage
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| FeatureError::InvalidPrepared("staging name is invalid".into()))?
            .to_owned();
        let now = timestamp();
        let pending = PendingFeatureSetPromotion {
            staging_name,
            feature_set: metadata_from_prepared(prepared),
            inputs: manifest
                .identity
                .inputs
                .iter()
                .map(|input| FeatureSetInput {
                    dataset_id: input.dataset_id.clone(),
                    dataset_fingerprint: input.fingerprint.clone(),
                })
                .collect(),
            attempt: FeatureAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                status: "MATERIALIZED".into(),
                feature_set_id: Some(manifest.feature_set_id.clone()),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        };
        {
            let connection = self.connection_guard()?;
            save_pending_feature_set_promotion(&connection, &pending)?;
        }
        if let Err(error) = promote_with_bounded_retry(&stage, &destination) {
            if !path_exists(&destination)? {
                let connection = self.connection_guard()?;
                clear_pending_feature_set_promotion(&connection, &manifest.feature_set_id)?;
            }
            return Err(error);
        }
        let mut connection = self.connection_guard()?;
        complete_pending_feature_set_promotion(&mut connection, &manifest.feature_set_id)?;
        Ok(FeatureCommitResult {
            feature_set_id: manifest.feature_set_id.clone(),
            status: manifest.status.clone(),
            disposition: FeatureCommitDisposition::Materialized,
        })
    }

    /// Removes a preview that the Owner did not confirm and records the cancellation.
    pub fn discard_prepared(&self, prepared: PreparedFeatureSet) -> Result<(), FeatureError> {
        let _operation = self.operation_guard()?;
        verify_prepared(&prepared, &self.staging_directory)?;
        fs::remove_dir_all(&prepared.staging_directory)?;
        self.save_attempt(
            "CANCELLED",
            Some(&prepared.manifest.feature_set_id),
            Some("OWNER_CANCELLED_PREVIEW"),
            Some("Feature set preview was cancelled before explicit confirmation"),
        )
    }

    pub fn record_failure(
        &self,
        status: &str,
        diagnostic_code: &str,
        diagnostic_message: &str,
    ) -> Result<(), FeatureError> {
        if !matches!(status, "FAILED" | "CANCELLED") {
            return Err(FeatureError::InvalidRequest(
                "only failed or cancelled attempts can be recorded as failures".into(),
            ));
        }
        let _operation = self.operation_guard()?;
        self.save_attempt(
            status,
            None,
            Some(diagnostic_code),
            Some(diagnostic_message),
        )
    }

    pub fn list_attempts(&self) -> Result<Vec<FeatureAttemptRecord>, FeatureError> {
        let connection = self.connection_guard()?;
        Ok(list_feature_attempts(&connection, ATTEMPT_HISTORY_LIMIT)?)
    }

    pub fn list_feature_sets(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<FeatureSetCatalogEntry>, FeatureError> {
        let mut entries = Vec::new();
        let mut cursor = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(FeatureError::Cancelled);
            }
            let page = {
                let connection = self.connection_guard()?;
                list_feature_set_metadata_after(&connection, &cursor, CATALOG_PAGE_SIZE)?
            };
            if page.is_empty() {
                return Ok(entries);
            }
            for metadata in page {
                cursor.clone_from(&metadata.feature_set_id);
                entries.push(self.verify_metadata(&metadata, cancel)?);
            }
        }
    }

    pub fn inspect_feature_set(
        &self,
        feature_set_id: &str,
    ) -> Result<Option<FeatureSetCatalogEntry>, FeatureError> {
        validate_feature_set_id(feature_set_id)?;
        let metadata = {
            let connection = self.connection_guard()?;
            get_feature_set_metadata(&connection, feature_set_id)?
        };
        metadata
            .map(|item| self.verify_metadata(&item, &AtomicBool::new(false)))
            .transpose()
    }

    /// Path of a registered feature set's output file, for verified downstream reads.
    pub fn output_path(&self, feature_set_id: &str) -> Result<PathBuf, FeatureError> {
        validate_feature_set_id(feature_set_id)?;
        Ok(self
            .feature_sets_directory
            .join(feature_set_id)
            .join(OUTPUT_FILE))
    }

    fn verify_metadata(
        &self,
        metadata: &FeatureSetMetadata,
        cancel: &AtomicBool,
    ) -> Result<FeatureSetCatalogEntry, FeatureError> {
        let manifest = serde_json::from_str::<FeatureSetManifest>(&metadata.manifest_json).ok();
        let directory = self.feature_sets_directory.join(&metadata.feature_set_id);
        let id_valid = validate_feature_set_id(&metadata.feature_set_id).is_ok();
        let identity_valid = manifest.as_ref().is_some_and(|manifest| {
            manifest.identity_is_valid()
                && manifest.feature_set_id == metadata.feature_set_id
                && manifest.identity_sha256 == metadata.identity_sha256
                && manifest.output_sha256 == metadata.output_sha256
                && manifest.status.as_str() == metadata.status
                && manifest
                    .canonical_json()
                    .is_ok_and(|json| json == metadata.manifest_json)
        });
        let manifest_file_valid = id_valid
            && directory_is_regular(&directory)
            && regular_file_bytes(&directory.join(MANIFEST_FILE))?
                .is_some_and(|bytes| bytes == metadata.manifest_json.as_bytes());
        let output_valid = id_valid
            && directory_is_regular(&directory)
            && is_regular_file(&directory.join(OUTPUT_FILE))?
            && engine::sha256_file(&directory.join(OUTPUT_FILE), cancel)? == metadata.output_sha256;
        let lineage_valid = match &manifest {
            Some(manifest) => self.lineage_matches(&metadata.feature_set_id, manifest)?,
            None => false,
        };
        let integrity = FeatureSetIntegrity {
            manifest_valid: metadata.manifest_integrity_valid
                && identity_valid
                && manifest_file_valid,
            output_valid,
            lineage_valid,
        };
        let valid = integrity.is_valid();
        Ok(FeatureSetCatalogEntry {
            feature_set_id: metadata.feature_set_id.clone(),
            identity_sha256: metadata.identity_sha256.clone(),
            manifest_sha256: metadata.manifest_sha256.clone(),
            status: match (&manifest, valid) {
                (Some(manifest), true) => manifest.status.clone(),
                _ => FeatureSetStatus::Quarantined,
            },
            manifest,
            diagnostic: (!valid).then(|| {
                "manifest, output digest, or dataset lineage does not match the immutable registry"
                    .into()
            }),
            integrity,
        })
    }

    fn lineage_matches(
        &self,
        feature_set_id: &str,
        manifest: &FeatureSetManifest,
    ) -> Result<bool, FeatureError> {
        let connection = self.connection_guard()?;
        let recorded = list_feature_set_inputs(&connection, feature_set_id)?;
        let expected: Vec<_> = manifest
            .identity
            .inputs
            .iter()
            .map(|input| FeatureSetInput {
                dataset_id: input.dataset_id.clone(),
                dataset_fingerprint: input.fingerprint.clone(),
            })
            .collect();
        if recorded != expected {
            return Ok(false);
        }
        for input in &recorded {
            let present =
                get_dataset_metadata(&connection, &input.dataset_id)?.is_some_and(|dataset| {
                    dataset.manifest_integrity_valid
                        && dataset.fingerprint == input.dataset_fingerprint
                        && dataset.status == "READY"
                });
            if !present {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn recover_pending_promotions(&self) -> Result<(), FeatureError> {
        let pending = {
            let connection = self.connection_guard()?;
            list_pending_feature_set_promotions(&connection)?
        };
        for item in pending {
            let id = item.feature_set.feature_set_id.clone();
            validate_feature_set_id(&id)?;
            let blocked = |reason: &str| FeatureError::RecoveryBlocked {
                feature_set_id: id.clone(),
                reason: reason.into(),
            };
            if Uuid::parse_str(&item.staging_name)
                .ok()
                .is_none_or(|uuid| uuid.to_string() != item.staging_name)
            {
                return Err(blocked("journal contains an invalid staging identity"));
            }
            let stage = self.staging_directory.join(&item.staging_name);
            let destination = self.feature_sets_directory.join(&id);
            match (path_exists(&stage)?, path_exists(&destination)?) {
                (true, true) => {
                    return Err(blocked(
                        "both staging and promoted directories exist; evidence was preserved",
                    ))
                }
                (false, true) => {
                    if !snapshot_matches(&destination, &item.feature_set)? {
                        return Err(blocked(
                            "promoted feature set failed journal integrity checks; evidence was preserved",
                        ));
                    }
                }
                (true, false) => {
                    if !snapshot_matches(&stage, &item.feature_set)? {
                        return Err(blocked(
                            "staged feature set failed journal integrity checks; evidence was preserved",
                        ));
                    }
                    promote_with_bounded_retry(&stage, &destination)?;
                    if !snapshot_matches(&destination, &item.feature_set)? {
                        return Err(blocked(
                            "promoted feature set failed post-move integrity checks; evidence was preserved",
                        ));
                    }
                }
                (false, false) => {
                    return Err(blocked(
                        "neither journaled staging nor promoted feature set exists; journal was preserved",
                    ))
                }
            }
            let mut connection = self.connection_guard()?;
            complete_pending_feature_set_promotion(&mut connection, &id)?;
        }
        Ok(())
    }

    fn has_pending_promotion(&self, feature_set_id: &str) -> Result<bool, FeatureError> {
        let connection = self.connection_guard()?;
        Ok(list_pending_feature_set_promotions(&connection)?
            .iter()
            .any(|item| item.feature_set.feature_set_id == feature_set_id))
    }

    fn save_attempt(
        &self,
        status: &str,
        feature_set_id: Option<&str>,
        diagnostic_code: Option<&str>,
        diagnostic_message: Option<&str>,
    ) -> Result<(), FeatureError> {
        let now = timestamp();
        let connection = self.connection_guard()?;
        save_feature_attempt(
            &connection,
            &FeatureAttemptRecord {
                attempt_id: Uuid::now_v7().to_string(),
                started_at_utc: now.clone(),
                completed_at_utc: now,
                status: status.into(),
                feature_set_id: feature_set_id.map(str::to_owned),
                diagnostic_code: diagnostic_code.map(str::to_owned),
                diagnostic_message: diagnostic_message.map(str::to_owned),
            },
        )?;
        Ok(())
    }

    fn connection_guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, FeatureError> {
        self.connection.lock().map_err(|_| FeatureError::Poisoned)
    }

    fn operation_guard(&self) -> Result<MutexGuard<'_, ()>, FeatureError> {
        self.operation_lock
            .lock()
            .map_err(|_| FeatureError::Poisoned)
    }

    #[doc(hidden)]
    pub fn staging_directory_for_tests(&self) -> &Path {
        &self.staging_directory
    }
}

fn prepare_in_stage(
    validated: &ValidatedRequest,
    inputs: &BTreeMap<String, ResolvedInput>,
    stage: &Path,
    cancel: &AtomicBool,
    progress: &FeatureProgress,
) -> Result<PreparedFeatureSet, FeatureError> {
    let materialized = engine::materialize(
        validated,
        inputs,
        &stage.join(OUTPUT_FILE),
        cancel,
        progress,
    )?;
    for (dataset_id, input) in inputs {
        if engine::sha256_file(&input.bars_path, cancel)? != input.manifest.canonical_bars_sha256 {
            return Err(FeatureError::InputIntegrity {
                dataset_id: dataset_id.clone(),
                reason: "canonical bars changed during feature computation".into(),
            });
        }
    }
    let identity = FeatureSetIdentity {
        schema_version: FEATURE_SET_SCHEMA_VERSION,
        engine_id: FEATURE_ENGINE_ID.into(),
        engine_version: FEATURE_ENGINE_VERSION.into(),
        temporal_semantics: TemporalSemantics::current(),
        target: validated.target.clone(),
        features: validated
            .features
            .iter()
            .map(|(feature, _)| feature.clone())
            .collect(),
        inputs: inputs
            .values()
            .map(|input| InputDatasetIdentity {
                dataset_id: input.manifest.dataset_id.clone(),
                fingerprint: input.manifest.fingerprint.clone(),
                canonical_bars_sha256: input.manifest.canonical_bars_sha256.clone(),
                canonical_bars_fingerprint: input.manifest.canonical_bars_fingerprint.clone(),
                bar_count: input.manifest.bar_count,
            })
            .collect(),
    };
    let identity_sha256 = identity.sha256()?;
    let mut series_used: BTreeMap<&str, std::collections::BTreeSet<String>> = BTreeMap::new();
    series_used
        .entry(validated.target.dataset_id.as_str())
        .or_default()
        .insert(format!(
            "{} {}",
            validated.target.symbol, validated.target.timeframe
        ));
    for (feature, _) in &validated.features {
        if let FeatureSource::Reference { series, .. } = &feature.source {
            series_used
                .entry(series.dataset_id.as_str())
                .or_default()
                .insert(format!("{} {}", series.symbol, series.timeframe));
        }
    }
    let lineage = inputs
        .values()
        .map(|input| {
            let manifest = &input.manifest;
            InputLineage {
                dataset_id: manifest.dataset_id.clone(),
                fingerprint: manifest.fingerprint.clone(),
                source_id: manifest.provenance.source_id.clone(),
                broker: manifest.provenance.broker.clone(),
                server: manifest.provenance.server.clone(),
                source_file_name: manifest.provenance.source_file_name.clone(),
                dataset_created_at_utc: manifest.created_at_utc.clone(),
                series_used: series_used
                    .get(manifest.dataset_id.as_str())
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default(),
            }
        })
        .collect();
    let status = if materialized
        .summaries
        .iter()
        .all(|summary| summary.ready_rows > 0)
    {
        FeatureSetStatus::Ready
    } else {
        FeatureSetStatus::Blocked
    };
    let manifest = FeatureSetManifest {
        schema_version: FEATURE_SET_SCHEMA_VERSION,
        feature_set_id: format!("fs_{identity_sha256}"),
        identity_sha256,
        identity,
        created_at_utc: timestamp(),
        output_file: OUTPUT_FILE.into(),
        output_sha256: materialized.output_sha256,
        output_fingerprint: materialized.output_fingerprint,
        row_count: materialized.row_count,
        columns: materialized.columns,
        feature_summaries: materialized.summaries,
        lineage,
        status,
        research_notice: RESEARCH_NOTICE.into(),
    };
    let manifest_json = manifest.canonical_json()?;
    let manifest_sha256 = hex(Sha256::digest(manifest_json.as_bytes()));
    fs::write(stage.join(MANIFEST_FILE), manifest_json.as_bytes())?;
    Ok(PreparedFeatureSet {
        preview: FeatureSetPreview {
            feature_set_id: manifest.feature_set_id.clone(),
            manifest_sha256: manifest_sha256.clone(),
            manifest: manifest.clone(),
            sample_rows: materialized.preview_rows,
        },
        manifest,
        manifest_json,
        manifest_sha256,
        staging_directory: stage.to_path_buf(),
    })
}

fn metadata_from_prepared(prepared: &PreparedFeatureSet) -> FeatureSetMetadata {
    FeatureSetMetadata {
        feature_set_id: prepared.manifest.feature_set_id.clone(),
        identity_sha256: prepared.manifest.identity_sha256.clone(),
        manifest_json: prepared.manifest_json.clone(),
        manifest_sha256: prepared.manifest_sha256.clone(),
        output_sha256: prepared.manifest.output_sha256.clone(),
        created_at_utc: prepared.manifest.created_at_utc.clone(),
        status: prepared.manifest.status.as_str().into(),
        manifest_integrity_valid: true,
    }
}

/// Checks that a prepared set is internally consistent and that its staged files still match.
/// Returns the canonical staging path.
fn verify_prepared(
    prepared: &PreparedFeatureSet,
    staging_root: &Path,
) -> Result<PathBuf, FeatureError> {
    let manifest = &prepared.manifest;
    if !manifest.identity_is_valid()
        || prepared.manifest_json != manifest.canonical_json()?
        || hex(Sha256::digest(prepared.manifest_json.as_bytes())) != prepared.manifest_sha256
        || prepared.preview.feature_set_id != manifest.feature_set_id
        || prepared.preview.manifest_sha256 != prepared.manifest_sha256
        || prepared.preview.manifest != *manifest
    {
        return Err(FeatureError::InvalidPrepared(
            "manifest, preview, or immutable identity is inconsistent".into(),
        ));
    }
    let canonical_root = fs::canonicalize(staging_root)?;
    let stage = fs::canonicalize(&prepared.staging_directory)?;
    let stage_name_valid = stage
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| {
            Uuid::parse_str(name)
                .ok()
                .map(|uuid| uuid.to_string() == name)
        })
        .unwrap_or(false);
    if stage.parent() != Some(canonical_root.as_path())
        || !directory_is_regular(&stage)
        || !stage_name_valid
    {
        return Err(FeatureError::InvalidPrepared(
            "staging path is outside the private feature staging directory".into(),
        ));
    }
    let never = AtomicBool::new(false);
    let manifest_matches = regular_file_bytes(&stage.join(MANIFEST_FILE))?
        .is_some_and(|bytes| bytes == prepared.manifest_json.as_bytes());
    let output_matches = is_regular_file(&stage.join(OUTPUT_FILE))?
        && engine::sha256_file(&stage.join(OUTPUT_FILE), &never)? == manifest.output_sha256;
    if !manifest_matches || !output_matches {
        return Err(FeatureError::InvalidPrepared(
            "staged manifest or feature output digest does not match".into(),
        ));
    }
    Ok(stage)
}

fn snapshot_matches(directory: &Path, metadata: &FeatureSetMetadata) -> Result<bool, FeatureError> {
    if !directory_is_regular(directory)
        || !metadata.manifest_integrity_valid
        || validate_feature_set_id(&metadata.feature_set_id).is_err()
        || !is_sha256(&metadata.output_sha256)
    {
        return Ok(false);
    }
    let Some(bytes) = regular_file_bytes(&directory.join(MANIFEST_FILE))? else {
        return Ok(false);
    };
    if bytes != metadata.manifest_json.as_bytes() {
        return Ok(false);
    }
    let Ok(manifest) = serde_json::from_slice::<FeatureSetManifest>(&bytes) else {
        return Ok(false);
    };
    if !manifest.identity_is_valid()
        || manifest.feature_set_id != metadata.feature_set_id
        || manifest.output_sha256 != metadata.output_sha256
        || manifest.canonical_json().ok().as_deref() != Some(metadata.manifest_json.as_str())
    {
        return Ok(false);
    }
    Ok(is_regular_file(&directory.join(OUTPUT_FILE))?
        && engine::sha256_file(&directory.join(OUTPUT_FILE), &AtomicBool::new(false))?
            == metadata.output_sha256)
}

fn ensure_store_directory(root: &Path, name: &str) -> Result<PathBuf, FeatureError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(FeatureError::UnsafeStorePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
        Err(error) => return Err(error.into()),
    }
    let canonical = fs::canonicalize(&path)?;
    if canonical.parent() != Some(root) {
        return Err(FeatureError::UnsafeStorePath);
    }
    Ok(canonical)
}

fn promote_with_bounded_retry(source: &Path, destination: &Path) -> Result<(), FeatureError> {
    let mut last_error = None;
    for attempt in 0..3u64 {
        if path_exists(destination)? {
            return Err(FeatureError::OrphanDirectoryExists);
        }
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
                ) && attempt < 2 =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(25 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(last_error
        .unwrap_or_else(|| std::io::Error::other("bounded promotion retry exhausted"))
        .into())
}

fn path_exists(path: &Path) -> Result<bool, FeatureError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn is_regular_file(path: &Path) -> Result<bool, FeatureError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn regular_file_bytes(path: &Path) -> Result<Option<Vec<u8>>, FeatureError> {
    if !is_regular_file(path)? {
        return Ok(None);
    }
    Ok(Some(fs::read(path)?))
}

fn validate_feature_set_id(value: &str) -> Result<(), FeatureError> {
    if value.len() != 67 || !value.starts_with("fs_") || !is_sha256(&value[3..]) {
        return Err(FeatureError::InvalidFeatureSetId);
    }
    Ok(())
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
