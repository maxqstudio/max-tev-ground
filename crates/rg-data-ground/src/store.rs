use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use rg_persistence::{
    clear_pending_dataset_promotion, complete_pending_dataset_promotion,
    get_dataset_by_fingerprint, get_dataset_metadata, list_dataset_metadata_after,
    list_import_attempts, list_pending_dataset_promotions, open_database, save_dataset_and_attempt,
    save_import_attempt, save_pending_dataset_promotion, DatasetMetadata, ImportAttemptRecord,
    PendingDatasetPromotion, PersistenceError,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;
use uuid::Uuid;

const CATALOG_METADATA_PAGE_SIZE: u64 = 32;

use super::{
    hash_file, hex_digest, prepare_dataset, DataGroundError, DatasetManifest, DatasetStatus,
    ImportRequest, PreparedDataset,
};

#[derive(Debug, Error)]
pub enum DatasetStoreError {
    #[error(transparent)]
    Data(#[from] DataGroundError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("dataset store filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("dataset store path is not a private regular directory or file")]
    UnsafeStorePath,
    #[error("dataset store is unavailable because its lock was poisoned")]
    Poisoned,
    #[error("prepared snapshot failed integrity verification: {0}")]
    InvalidPrepared(String),
    #[error("dataset ID is invalid")]
    InvalidDatasetId,
    #[error("dataset already exists but its stored content is not trustworthy")]
    ExistingDatasetCorrupt,
    #[error("an unregistered dataset directory already occupies this identity")]
    OrphanDirectoryExists,
    #[error("dataset promotion recovery is blocked for {dataset_id}: {reason}")]
    RecoveryBlocked { dataset_id: String, reason: String },
    #[error("another dataset catalogue verification is already active")]
    VerificationBusy,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommitDisposition {
    Imported,
    Reused,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImportCommitResult {
    pub dataset_id: String,
    pub fingerprint: String,
    pub status: DatasetStatus,
    pub disposition: CommitDisposition,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SnapshotIntegrity {
    pub manifest_valid: bool,
    pub raw_source_valid: bool,
    pub canonical_parquet_valid: bool,
}

impl SnapshotIntegrity {
    fn is_valid(&self) -> bool {
        self.manifest_valid && self.raw_source_valid && self.canonical_parquet_valid
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DatasetCatalogEntry {
    pub dataset_id: String,
    pub fingerprint: String,
    pub manifest_sha256: String,
    pub status: DatasetStatus,
    pub manifest: Option<DatasetManifest>,
    pub integrity: SnapshotIntegrity,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ImportAttemptSummary {
    pub attempt_id: String,
    pub started_at_utc: String,
    pub completed_at_utc: String,
    pub status: String,
    pub source_file_name: String,
    pub diagnostic_code: Option<String>,
    pub diagnostic_message: Option<String>,
}

pub struct DatasetStore {
    staging_directory: PathBuf,
    datasets_directory: PathBuf,
    connection: Mutex<rusqlite::Connection>,
    operation_lock: Mutex<()>,
    verification_lock: Mutex<()>,
}

impl DatasetStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, DatasetStoreError> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;
        let staging_directory = ensure_store_directory(&root, ".staging")?;
        let datasets_directory = ensure_store_directory(&root, "datasets")?;
        let database_path = root.join("research.sqlite3");
        match fs::symlink_metadata(&database_path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(DatasetStoreError::UnsafeStorePath),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let connection = open_database(database_path)?;
        let store = Self {
            staging_directory,
            datasets_directory,
            connection: Mutex::new(connection),
            operation_lock: Mutex::new(()),
            verification_lock: Mutex::new(()),
        };
        store.recover_pending_promotions()?;
        Ok(store)
    }

    pub fn prepare_import(
        &self,
        source: impl AsRef<Path>,
        request: ImportRequest,
        cancel: std::sync::Arc<AtomicBool>,
    ) -> Result<PreparedDataset, DatasetStoreError> {
        Ok(prepare_dataset(
            source,
            &self.staging_directory,
            request,
            cancel,
        )?)
    }

    pub fn commit_prepared(
        &self,
        prepared: PreparedDataset,
    ) -> Result<ImportCommitResult, DatasetStoreError> {
        let _operation = self.operation_guard()?;
        let _verification = self
            .verification_lock
            .lock()
            .map_err(|_| DatasetStoreError::Poisoned)?;
        let stage = prepared.staging_directory.clone();
        let dataset_id = prepared.manifest.dataset_id.clone();
        let source_name = prepared.manifest.provenance.source_file_name.clone();
        let result = self.commit_prepared_locked(&prepared);
        if let Err(error) = &result {
            if !self.has_pending_promotion(&dataset_id).unwrap_or(true) {
                let status = if matches!(error, DatasetStoreError::Data(DataGroundError::Cancelled))
                {
                    "CANCELLED"
                } else {
                    "FAILED"
                };
                let _ = self.record_attempt_locked(
                    status,
                    &source_name,
                    Some(store_error_code(error).into()),
                    Some(error.to_string()),
                );
            }
        }
        if stage.exists() && !self.has_pending_promotion(&dataset_id).unwrap_or(true) {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    fn commit_prepared_locked(
        &self,
        prepared: &PreparedDataset,
    ) -> Result<ImportCommitResult, DatasetStoreError> {
        verify_prepared(prepared, &self.staging_directory)?;
        let manifest = &prepared.manifest;
        let existing = {
            let connection = self.connection_guard()?;
            get_dataset_by_fingerprint(&connection, &manifest.fingerprint)?
        };
        if let Some(existing) = existing {
            let entry = self.verify_metadata(&existing, &AtomicBool::new(false))?;
            if !entry.integrity.is_valid() || entry.status == DatasetStatus::Quarantined {
                return Err(DatasetStoreError::ExistingDatasetCorrupt);
            }
            let connection = self.connection_guard()?;
            save_import_attempt(
                &connection,
                &terminal_attempt(
                    "REUSED",
                    &manifest.provenance.source_file_name,
                    Some("IDENTICAL_FINGERPRINT".into()),
                    Some(format!("Existing dataset {} reused", existing.dataset_id)),
                ),
            )?;
            return Ok(ImportCommitResult {
                dataset_id: existing.dataset_id,
                fingerprint: manifest.fingerprint.clone(),
                status: entry.status,
                disposition: CommitDisposition::Reused,
            });
        }
        let destination = self.datasets_directory.join(&manifest.dataset_id);
        if path_exists_without_following(&destination)? {
            if let Some(existing) = self.recover_unjournaled_orphan(&destination)? {
                if existing.fingerprint == manifest.fingerprint {
                    let attempt = terminal_attempt(
                        "REUSED",
                        &manifest.provenance.source_file_name,
                        Some("RECOVERED_ORPHAN_REUSED".into()),
                        Some(format!("Recovered dataset {} reused", existing.dataset_id)),
                    );
                    let connection = self.connection_guard()?;
                    save_import_attempt(&connection, &attempt)?;
                    return Ok(ImportCommitResult {
                        dataset_id: existing.dataset_id,
                        fingerprint: manifest.fingerprint.clone(),
                        status: manifest.status.clone(),
                        disposition: CommitDisposition::Reused,
                    });
                }
            }
            return Err(DatasetStoreError::OrphanDirectoryExists);
        }
        let metadata = metadata_from_prepared(prepared);
        let attempt = terminal_attempt(
            "IMPORTED",
            &manifest.provenance.source_file_name,
            None,
            None,
        );
        let stage_name = prepared
            .staging_directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| DatasetStoreError::InvalidPrepared("staging name is invalid".into()))?
            .to_owned();
        let canonical_stage_name = Uuid::parse_str(&stage_name)
            .ok()
            .map(|value| value.to_string());
        if canonical_stage_name.as_deref() != Some(stage_name.as_str()) {
            return Err(DatasetStoreError::InvalidPrepared(
                "staging name must be a canonical UUID".into(),
            ));
        }
        let pending = PendingDatasetPromotion {
            staging_name: stage_name,
            dataset: metadata,
            attempt,
        };
        {
            let connection = self.connection_guard()?;
            save_pending_dataset_promotion(&connection, &pending)?;
        }
        if let Err(error) = promote_with_bounded_retry(&prepared.staging_directory, &destination) {
            if !path_exists_without_following(&destination)? {
                let connection = self.connection_guard()?;
                clear_pending_dataset_promotion(&connection, &manifest.dataset_id)?;
            }
            return Err(error);
        }
        let mut connection = self.connection_guard()?;
        complete_pending_dataset_promotion(&mut connection, &manifest.dataset_id)?;
        Ok(ImportCommitResult {
            dataset_id: manifest.dataset_id.clone(),
            fingerprint: manifest.fingerprint.clone(),
            status: manifest.status.clone(),
            disposition: CommitDisposition::Imported,
        })
    }

    pub fn list_datasets(&self) -> Result<Vec<DatasetCatalogEntry>, DatasetStoreError> {
        let cancel = AtomicBool::new(false);
        self.list_datasets_with_progress(&cancel, |_, _, _| {})
    }

    pub fn list_datasets_with_progress<F>(
        &self,
        cancel: &AtomicBool,
        mut on_entry: F,
    ) -> Result<Vec<DatasetCatalogEntry>, DatasetStoreError>
    where
        F: FnMut(usize, usize, &DatasetCatalogEntry),
    {
        let _verification = self
            .verification_lock
            .try_lock()
            .map_err(|_| DatasetStoreError::VerificationBusy)?;
        let total = self.dataset_count()?;
        let mut entries = Vec::new();
        let mut cursor = String::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DataGroundError::Cancelled.into());
            }
            let metadata = {
                let connection = self.connection_guard()?;
                list_dataset_metadata_after(&connection, &cursor, CATALOG_METADATA_PAGE_SIZE)?
            };
            if metadata.is_empty() {
                break;
            }
            for item in metadata {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DataGroundError::Cancelled.into());
                }
                cursor.clone_from(&item.dataset_id);
                let entry = self.verify_metadata(&item, cancel)?;
                entries.push(entry);
                on_entry(
                    entries.len(),
                    total,
                    entries.last().expect("entry was just added"),
                );
            }
        }
        Ok(entries)
    }

    pub fn dataset_count(&self) -> Result<usize, DatasetStoreError> {
        let connection = self.connection_guard()?;
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM datasets", [], |row| row.get(0))
            .map_err(PersistenceError::from)?;
        Ok(count as usize)
    }

    pub fn inspect_dataset(
        &self,
        dataset_id: &str,
    ) -> Result<Option<DatasetCatalogEntry>, DatasetStoreError> {
        validate_dataset_id(dataset_id)?;
        let _verification = self
            .verification_lock
            .try_lock()
            .map_err(|_| DatasetStoreError::VerificationBusy)?;
        let metadata = {
            let connection = self.connection_guard()?;
            get_dataset_metadata(&connection, dataset_id)?
        };
        let cancel = AtomicBool::new(false);
        metadata
            .map(|item| self.verify_metadata(&item, &cancel))
            .transpose()
    }

    /// Path of a registered snapshot's canonical bars file. Callers must verify the dataset with
    /// [`DatasetStore::inspect_dataset`] and re-check the file digest around every read.
    pub fn canonical_bars_path(&self, dataset_id: &str) -> Result<PathBuf, DatasetStoreError> {
        validate_dataset_id(dataset_id)?;
        Ok(self
            .datasets_directory
            .join(dataset_id)
            .join("bars.parquet"))
    }

    pub fn list_attempts(&self) -> Result<Vec<ImportAttemptSummary>, DatasetStoreError> {
        let connection = self.connection_guard()?;
        Ok(list_import_attempts(&connection)?
            .into_iter()
            .map(|attempt| ImportAttemptSummary {
                attempt_id: attempt.attempt_id,
                started_at_utc: attempt.started_at_utc,
                completed_at_utc: attempt.completed_at_utc,
                status: attempt.status,
                source_file_name: attempt.source_file_name,
                diagnostic_code: attempt.diagnostic_code,
                diagnostic_message: attempt.diagnostic_message,
            })
            .collect())
    }

    pub fn record_failure(
        &self,
        source_file_name: &str,
        status: &str,
        diagnostic_code: &str,
        diagnostic_message: &str,
    ) -> Result<(), DatasetStoreError> {
        if !matches!(status, "FAILED" | "CANCELLED") {
            return Err(DataGroundError::InvalidRequest(
                "only failed or cancelled imports can be recorded as failures".into(),
            )
            .into());
        }
        let _operation = self.operation_guard()?;
        self.record_attempt_locked(
            status,
            source_file_name,
            Some(diagnostic_code.to_owned()),
            Some(diagnostic_message.to_owned()),
        )
    }

    fn verify_metadata(
        &self,
        metadata: &DatasetMetadata,
        cancel: &AtomicBool,
    ) -> Result<DatasetCatalogEntry, DatasetStoreError> {
        let manifest_integrity = metadata.manifest_integrity_valid;
        let parsed = serde_json::from_str::<DatasetManifest>(&metadata.manifest_json);
        let manifest = parsed.ok();
        let identity_valid = manifest.as_ref().is_some_and(|manifest| {
            manifest.dataset_id == metadata.dataset_id
                && manifest.fingerprint == metadata.fingerprint
                && manifest.provenance.raw_sha256 == metadata.raw_sha256
                && manifest.canonical_bars_sha256 == metadata.parquet_sha256
                && manifest.status.as_str() == metadata.status
                && manifest.canonical_bars_file == "bars.parquet"
                && manifest.fingerprint_is_valid()
        });
        let directory = self.datasets_directory.join(&metadata.dataset_id);
        let (manifest_file_valid, raw_source_valid, canonical_parquet_valid) =
            if validate_dataset_id(&metadata.dataset_id).is_ok() && directory_is_regular(&directory)
            {
                let manifest_valid = read_verified_file(
                    &directory.join("manifest.json"),
                    &metadata.manifest_sha256,
                    cancel,
                )?;
                let raw_valid = if let Some(name) = manifest
                    .as_ref()
                    .filter(|value| {
                        matches!(value.provenance.source_format.as_str(), "csv" | "parquet")
                    })
                    .map(|value| format!("raw-source.{}", value.provenance.source_format))
                {
                    read_verified_file(&directory.join(name), &metadata.raw_sha256, cancel)?
                } else {
                    false
                };
                let parquet_valid = read_verified_file(
                    &directory.join("bars.parquet"),
                    &metadata.parquet_sha256,
                    cancel,
                )?;
                (manifest_valid, raw_valid, parquet_valid)
            } else {
                (false, false, false)
            };
        let manifest_file_matches = validate_dataset_id(&metadata.dataset_id).is_ok()
            && manifest.as_ref().is_some_and(|value| {
                fs::read(directory.join("manifest.json"))
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .is_some_and(|contents| {
                        value.canonical_json().is_ok_and(|json| json == contents)
                    })
            });
        let integrity = SnapshotIntegrity {
            manifest_valid: manifest_integrity
                && identity_valid
                && manifest_file_valid
                && manifest_file_matches,
            raw_source_valid: raw_source_valid
                && manifest.as_ref().is_some_and(|value| {
                    fs::metadata(
                        directory.join(format!("raw-source.{}", value.provenance.source_format)),
                    )
                    .is_ok_and(|metadata| metadata.len() == value.provenance.source_size_bytes)
                }),
            canonical_parquet_valid,
        };
        let integrity_valid = integrity.is_valid();
        let status = if integrity_valid {
            manifest
                .as_ref()
                .map(|value| value.status.clone())
                .unwrap_or(DatasetStatus::Quarantined)
        } else {
            DatasetStatus::Quarantined
        };
        Ok(DatasetCatalogEntry {
            dataset_id: metadata.dataset_id.clone(),
            fingerprint: metadata.fingerprint.clone(),
            manifest_sha256: metadata.manifest_sha256.clone(),
            status,
            manifest,
            integrity,
            diagnostic: if integrity_valid {
                None
            } else {
                Some(
                    "manifest or snapshot file digest does not match the immutable registry".into(),
                )
            },
        })
    }

    fn recover_pending_promotions(&self) -> Result<(), DatasetStoreError> {
        let pending = {
            let connection = self.connection_guard()?;
            list_pending_dataset_promotions(&connection)?
        };
        for item in pending {
            validate_dataset_id(&item.dataset.dataset_id)?;
            let stage_uuid = Uuid::parse_str(&item.staging_name).ok();
            if stage_uuid.is_none_or(|uuid| uuid.to_string() != item.staging_name) {
                return Err(recovery_blocked(
                    &item.dataset.dataset_id,
                    "journal contains an invalid staging identity",
                ));
            }
            let stage = self.staging_directory.join(&item.staging_name);
            let destination = self.datasets_directory.join(&item.dataset.dataset_id);
            let stage_exists = path_exists_without_following(&stage)?;
            let destination_exists = path_exists_without_following(&destination)?;
            if stage_exists && destination_exists {
                return Err(recovery_blocked(
                    &item.dataset.dataset_id,
                    "both staging and promoted directories exist; evidence was preserved",
                ));
            }
            if destination_exists {
                if !snapshot_matches_metadata(&destination, &item.dataset, &AtomicBool::new(false))?
                {
                    return Err(recovery_blocked(
                        &item.dataset.dataset_id,
                        "promoted snapshot failed journal integrity checks; evidence was preserved",
                    ));
                }
            } else if stage_exists {
                if !snapshot_matches_metadata(&stage, &item.dataset, &AtomicBool::new(false))? {
                    return Err(recovery_blocked(
                        &item.dataset.dataset_id,
                        "staged snapshot failed journal integrity checks; evidence was preserved",
                    ));
                }
                promote_with_bounded_retry(&stage, &destination)?;
                if !snapshot_matches_metadata(&destination, &item.dataset, &AtomicBool::new(false))?
                {
                    return Err(recovery_blocked(
                        &item.dataset.dataset_id,
                        "promoted snapshot failed post-move integrity checks; evidence was preserved",
                    ));
                }
            } else {
                return Err(recovery_blocked(
                    &item.dataset.dataset_id,
                    "neither journaled staging nor promoted snapshot exists; journal was preserved",
                ));
            }
            let mut connection = self.connection_guard()?;
            complete_pending_dataset_promotion(&mut connection, &item.dataset.dataset_id)?;
        }
        Ok(())
    }

    fn recover_unjournaled_orphan(
        &self,
        directory: &Path,
    ) -> Result<Option<DatasetMetadata>, DatasetStoreError> {
        let Some(metadata) = metadata_from_snapshot_directory(directory)? else {
            return Ok(None);
        };
        let source_file_name = serde_json::from_str::<DatasetManifest>(&metadata.manifest_json)
            .map(|manifest| manifest.provenance.source_file_name)
            .map_err(|error| {
                recovery_blocked(
                    &metadata.dataset_id,
                    &format!("verified orphan manifest could not be parsed: {error}"),
                )
            })?;
        let attempt = terminal_attempt(
            "IMPORTED",
            &source_file_name,
            Some("RECOVERED_UNJOURNALED_PROMOTION".into()),
            Some(
                "Recovered a fully verified snapshot left by an earlier interrupted commit".into(),
            ),
        );
        let mut connection = self.connection_guard()?;
        save_dataset_and_attempt(&mut connection, &metadata, &attempt)?;
        Ok(Some(metadata))
    }

    fn has_pending_promotion(&self, dataset_id: &str) -> Result<bool, DatasetStoreError> {
        let connection = self.connection_guard()?;
        Ok(list_pending_dataset_promotions(&connection)?
            .iter()
            .any(|item| item.dataset.dataset_id == dataset_id))
    }

    fn record_attempt_locked(
        &self,
        status: &str,
        source_file_name: &str,
        diagnostic_code: Option<String>,
        diagnostic_message: Option<String>,
    ) -> Result<(), DatasetStoreError> {
        let record = terminal_attempt(
            status,
            source_file_name,
            diagnostic_code,
            diagnostic_message,
        );
        let connection = self.connection_guard()?;
        save_import_attempt(&connection, &record)?;
        Ok(())
    }

    fn connection_guard(&self) -> Result<MutexGuard<'_, rusqlite::Connection>, DatasetStoreError> {
        self.connection
            .lock()
            .map_err(|_| DatasetStoreError::Poisoned)
    }

    fn operation_guard(&self) -> Result<MutexGuard<'_, ()>, DatasetStoreError> {
        self.operation_lock
            .lock()
            .map_err(|_| DatasetStoreError::Poisoned)
    }
}

fn ensure_store_directory(root: &Path, name: &str) -> Result<PathBuf, DatasetStoreError> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(DatasetStoreError::UnsafeStorePath),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
        Err(error) => return Err(error.into()),
    }
    let canonical = fs::canonicalize(&path)?;
    if canonical.parent() != Some(root) {
        return Err(DatasetStoreError::UnsafeStorePath);
    }
    Ok(canonical)
}

fn metadata_from_prepared(prepared: &PreparedDataset) -> DatasetMetadata {
    DatasetMetadata {
        dataset_id: prepared.manifest.dataset_id.clone(),
        fingerprint: prepared.manifest.fingerprint.clone(),
        manifest_json: prepared.manifest_json.clone(),
        manifest_sha256: prepared.manifest_sha256.clone(),
        parquet_sha256: prepared.manifest.canonical_bars_sha256.clone(),
        raw_sha256: prepared.manifest.provenance.raw_sha256.clone(),
        created_at_utc: prepared.manifest.created_at_utc.clone(),
        status: prepared.manifest.status.as_str().into(),
        manifest_integrity_valid: true,
    }
}

fn verify_prepared(
    prepared: &PreparedDataset,
    staging_root: &Path,
) -> Result<(), DatasetStoreError> {
    validate_dataset_id(&prepared.manifest.dataset_id)?;
    if prepared.manifest.dataset_id != format!("ds_{}", prepared.manifest.fingerprint)
        || !is_sha256(&prepared.manifest.fingerprint)
        || !prepared.manifest.fingerprint_is_valid()
        || !is_sha256(&prepared.manifest_sha256)
        || prepared.manifest.canonical_bars_file != "bars.parquet"
        || prepared.manifest_json != prepared.manifest.canonical_json()?
        || hex_digest(sha2::Sha256::digest(prepared.manifest_json.as_bytes()))
            != prepared.manifest_sha256
        || prepared.preview.dataset_id != prepared.manifest.dataset_id
        || prepared.preview.fingerprint != prepared.manifest.fingerprint
        || prepared.preview.manifest_sha256 != prepared.manifest_sha256
        || prepared.preview.raw_sha256 != prepared.manifest.provenance.raw_sha256
        || prepared.preview.canonical_bars_sha256 != prepared.manifest.canonical_bars_sha256
        || prepared.preview.canonical_bars_fingerprint
            != prepared.manifest.canonical_bars_fingerprint
    {
        return Err(DatasetStoreError::InvalidPrepared(
            "manifest, preview, or immutable identity is inconsistent".into(),
        ));
    }
    let canonical_root = fs::canonicalize(staging_root)?;
    let canonical_stage = fs::canonicalize(&prepared.staging_directory)?;
    if canonical_stage.parent() != Some(canonical_root.as_path())
        || !directory_is_regular(&canonical_stage)
    {
        return Err(DatasetStoreError::InvalidPrepared(
            "staging path is outside the private staging directory".into(),
        ));
    }
    let raw_name = format!("raw-source.{}", prepared.manifest.provenance.source_format);
    let never_cancel = AtomicBool::new(false);
    let raw_valid = read_verified_file(
        &canonical_stage.join(raw_name),
        &prepared.manifest.provenance.raw_sha256,
        &never_cancel,
    )?;
    let parquet_valid = read_verified_file(
        &canonical_stage.join("bars.parquet"),
        &prepared.manifest.canonical_bars_sha256,
        &never_cancel,
    )?;
    let manifest_valid = read_verified_file(
        &canonical_stage.join("manifest.json"),
        &prepared.manifest_sha256,
        &never_cancel,
    )?;
    if !raw_valid || !parquet_valid || !manifest_valid {
        return Err(DatasetStoreError::InvalidPrepared(
            "staged manifest, source copy, or Parquet snapshot digest does not match".into(),
        ));
    }
    Ok(())
}

fn snapshot_matches_metadata(
    directory: &Path,
    metadata: &DatasetMetadata,
    cancel: &AtomicBool,
) -> Result<bool, DatasetStoreError> {
    if !directory_is_regular(directory)
        || !metadata.manifest_integrity_valid
        || !validate_dataset_id(&metadata.dataset_id).is_ok()
        || metadata.dataset_id != format!("ds_{}", metadata.fingerprint)
        || !is_sha256(&metadata.fingerprint)
        || !is_sha256(&metadata.manifest_sha256)
        || !is_sha256(&metadata.raw_sha256)
        || !is_sha256(&metadata.parquet_sha256)
    {
        return Ok(false);
    }
    let bytes = match fs::read(directory.join("manifest.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if bytes != metadata.manifest_json.as_bytes()
        || hex_digest(sha2::Sha256::digest(&bytes)) != metadata.manifest_sha256
    {
        return Ok(false);
    }
    let Ok(manifest) = serde_json::from_slice::<DatasetManifest>(&bytes) else {
        return Ok(false);
    };
    let canonical = match manifest.canonical_json() {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    if canonical.as_bytes() != bytes
        || !manifest.fingerprint_is_valid()
        || manifest.dataset_id != metadata.dataset_id
        || manifest.fingerprint != metadata.fingerprint
        || manifest.provenance.raw_sha256 != metadata.raw_sha256
        || manifest.canonical_bars_sha256 != metadata.parquet_sha256
        || manifest.canonical_bars_file != "bars.parquet"
        || manifest.status.as_str() != metadata.status
        || !matches!(
            manifest.provenance.source_format.as_str(),
            "csv" | "parquet"
        )
    {
        return Ok(false);
    }
    let raw_path = directory.join(format!("raw-source.{}", manifest.provenance.source_format));
    let raw_valid = read_verified_file(&raw_path, &metadata.raw_sha256, cancel)?
        && fs::metadata(&raw_path)
            .is_ok_and(|file| file.len() == manifest.provenance.source_size_bytes);
    if !raw_valid {
        return Ok(false);
    }
    read_verified_file(
        &directory.join("bars.parquet"),
        &metadata.parquet_sha256,
        cancel,
    )
}

fn metadata_from_snapshot_directory(
    directory: &Path,
) -> Result<Option<DatasetMetadata>, DatasetStoreError> {
    if !directory_is_regular(directory) {
        return Ok(None);
    }
    let bytes = match fs::read(directory.join("manifest.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Ok(manifest_json) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let Ok(manifest) = serde_json::from_str::<DatasetManifest>(&manifest_json) else {
        return Ok(None);
    };
    if manifest.canonical_json().ok().as_deref() != Some(&manifest_json)
        || !manifest.fingerprint_is_valid()
        || directory.file_name().and_then(|name| name.to_str()) != Some(&manifest.dataset_id)
    {
        return Ok(None);
    }
    let metadata = DatasetMetadata {
        dataset_id: manifest.dataset_id.clone(),
        fingerprint: manifest.fingerprint.clone(),
        manifest_sha256: hex_digest(sha2::Sha256::digest(manifest_json.as_bytes())),
        parquet_sha256: manifest.canonical_bars_sha256.clone(),
        raw_sha256: manifest.provenance.raw_sha256.clone(),
        created_at_utc: manifest.created_at_utc.clone(),
        status: manifest.status.as_str().into(),
        manifest_integrity_valid: true,
        manifest_json,
    };
    if snapshot_matches_metadata(directory, &metadata, &AtomicBool::new(false))? {
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

fn path_exists_without_following(path: &Path) -> Result<bool, DatasetStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn recovery_blocked(dataset_id: &str, reason: &str) -> DatasetStoreError {
    DatasetStoreError::RecoveryBlocked {
        dataset_id: dataset_id.into(),
        reason: reason.into(),
    }
}

fn promote_with_bounded_retry(source: &Path, destination: &Path) -> Result<(), DatasetStoreError> {
    let mut last_error = None;
    for attempt in 0..3 {
        if path_exists_without_following(destination)? {
            return Err(DatasetStoreError::OrphanDirectoryExists);
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

fn terminal_attempt(
    status: &str,
    source_file_name: &str,
    diagnostic_code: Option<String>,
    diagnostic_message: Option<String>,
) -> ImportAttemptRecord {
    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    ImportAttemptRecord {
        attempt_id: Uuid::now_v7().to_string(),
        started_at_utc: timestamp.clone(),
        completed_at_utc: timestamp,
        status: status.into(),
        source_file_name: source_file_name.into(),
        diagnostic_code,
        diagnostic_message,
    }
}

fn store_error_code(error: &DatasetStoreError) -> &'static str {
    match error {
        DatasetStoreError::Data(DataGroundError::Cancelled) => "CANCELLED",
        DatasetStoreError::Data(DataGroundError::ResourceQuota(_)) => "RESOURCE_QUOTA",
        DatasetStoreError::Data(DataGroundError::InvalidRequest(_)) => "INVALID_REQUEST",
        DatasetStoreError::Data(DataGroundError::InvalidRow { .. }) => "INVALID_ROW",
        DatasetStoreError::Data(DataGroundError::DuplicateBar { .. }) => "DUPLICATE_BAR",
        DatasetStoreError::Data(DataGroundError::OutOfOrderBar { .. }) => "OUT_OF_ORDER_BAR",
        DatasetStoreError::InvalidPrepared(_) => "INVALID_PREPARED_SNAPSHOT",
        DatasetStoreError::ExistingDatasetCorrupt => "EXISTING_DATASET_CORRUPT",
        DatasetStoreError::OrphanDirectoryExists => "ORPHAN_DIRECTORY_EXISTS",
        DatasetStoreError::RecoveryBlocked { .. } => "PROMOTION_RECOVERY_BLOCKED",
        DatasetStoreError::VerificationBusy => "CATALOG_VERIFICATION_BUSY",
        DatasetStoreError::InvalidDatasetId => "INVALID_DATASET_ID",
        DatasetStoreError::Persistence(_) => "PERSISTENCE_FAILURE",
        DatasetStoreError::Io(_) => "FILESYSTEM_FAILURE",
        DatasetStoreError::UnsafeStorePath => "UNSAFE_STORE_PATH",
        DatasetStoreError::Poisoned => "STORE_UNAVAILABLE",
        DatasetStoreError::Data(_) => "IMPORT_FAILURE",
    }
}

fn validate_dataset_id(value: &str) -> Result<(), DatasetStoreError> {
    if value.len() != 67
        || !value.starts_with("ds_")
        || !value[3..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DatasetStoreError::InvalidDatasetId);
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn directory_is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn read_verified_file(
    path: &Path,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<bool, DatasetStoreError> {
    if !is_sha256(expected_sha256) {
        return Ok(false);
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    Ok(hash_file(path, cancel)? == expected_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CalendarPolicy, CsvDelimiter, FieldMapping, MarketRole, ResourceQuotas, TimestampSemantics,
    };
    use std::{collections::BTreeMap, fs, sync::Arc};
    use tempfile::TempDir;

    const FIXTURE: &str = include_str!("../fixtures/eurusd-gbpusd-h1-h4.csv");

    fn request() -> ImportRequest {
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
            source_id: "store-test-fixture".into(),
            broker: "FIXTURE".into(),
            server: "FIXTURE-SERVER".into(),
            delimiter: CsvDelimiter::Comma,
            calendar: CalendarPolicy::WeekendsUtc,
            quotas: ResourceQuotas {
                max_source_bytes: Some(10_000_000),
                max_records: Some(1000),
            },
        }
    }

    fn create_fixture(directory: &TempDir, name: &str) -> PathBuf {
        let path = directory.path().join(name);
        fs::write(&path, FIXTURE).unwrap();
        path
    }

    fn staged(store: &DatasetStore, source: &Path) -> PreparedDataset {
        store
            .prepare_import(
                source,
                request(),
                std::sync::Arc::new(AtomicBool::new(false)),
            )
            .unwrap()
    }

    #[test]
    fn promote_reopen_and_read_verify_registry_and_snapshot() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let source = create_fixture(&directory, "bars.csv");
        let prepared = staged(&store, &source);
        let id = prepared.manifest.dataset_id.clone();
        let result = store.commit_prepared(prepared).unwrap();
        assert_eq!(result.disposition, CommitDisposition::Imported);
        assert_eq!(result.status, DatasetStatus::Blocked);
        let item = store.inspect_dataset(&id).unwrap().unwrap();
        assert_eq!(item.status, DatasetStatus::Blocked);
        assert!(item.integrity.is_valid());
        assert_eq!(item.manifest.unwrap().bar_count, 5);
        assert_eq!(store.list_datasets().unwrap().len(), 1);

        let reopened = DatasetStore::open(directory.path().join("store")).unwrap();
        assert_eq!(
            reopened.inspect_dataset(&id).unwrap().unwrap().status,
            DatasetStatus::Blocked
        );
        assert_eq!(reopened.list_attempts().unwrap().len(), 1);
    }

    #[test]
    fn restart_recovers_a_crash_after_snapshot_rename_and_retry_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let source = create_fixture(&directory, "crash-after-rename.csv");
        let store = DatasetStore::open(&root).unwrap();
        let prepared = staged(&store, &source);
        let dataset_id = prepared.manifest.dataset_id.clone();
        let stage_name = prepared
            .staging_directory
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let pending = PendingDatasetPromotion {
            staging_name: stage_name,
            dataset: metadata_from_prepared(&prepared),
            attempt: terminal_attempt(
                "IMPORTED",
                &prepared.manifest.provenance.source_file_name,
                None,
                None,
            ),
        };
        {
            let connection = store.connection_guard().unwrap();
            save_pending_dataset_promotion(&connection, &pending).unwrap();
        }
        let destination = store.datasets_directory.join(&dataset_id);
        promote_with_bounded_retry(&prepared.staging_directory, &destination).unwrap();
        drop(store);

        let reopened = DatasetStore::open(&root).unwrap();
        let recovered = reopened.inspect_dataset(&dataset_id).unwrap().unwrap();
        assert!(recovered.integrity.is_valid());
        assert_eq!(recovered.status, DatasetStatus::Blocked);
        let result = reopened
            .commit_prepared(staged(&reopened, &source))
            .unwrap();
        assert_eq!(result.disposition, CommitDisposition::Reused);
        assert_eq!(reopened.list_datasets().unwrap().len(), 1);
        let attempts = reopened.list_attempts().unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().any(|item| item.status == "IMPORTED"));
        assert!(attempts.iter().any(|item| item.status == "REUSED"));
    }

    #[test]
    fn restart_recovers_a_journaled_stage_before_snapshot_rename() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let source = create_fixture(&directory, "crash-before-rename.csv");
        let store = DatasetStore::open(&root).unwrap();
        let prepared = staged(&store, &source);
        let dataset_id = prepared.manifest.dataset_id.clone();
        let stage_name = prepared
            .staging_directory
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let pending = PendingDatasetPromotion {
            staging_name: stage_name,
            dataset: metadata_from_prepared(&prepared),
            attempt: terminal_attempt(
                "IMPORTED",
                &prepared.manifest.provenance.source_file_name,
                None,
                None,
            ),
        };
        {
            let connection = store.connection_guard().unwrap();
            save_pending_dataset_promotion(&connection, &pending).unwrap();
        }
        let staged_path = prepared.staging_directory.clone();
        drop(prepared);
        drop(store);

        let reopened = DatasetStore::open(&root).unwrap();
        assert!(!staged_path.exists());
        assert_eq!(reopened.list_datasets().unwrap().len(), 1);
        assert!(reopened
            .inspect_dataset(&dataset_id)
            .unwrap()
            .unwrap()
            .integrity
            .is_valid());
        assert_eq!(reopened.list_attempts().unwrap().len(), 1);
    }

    #[test]
    fn retry_recovers_a_valid_legacy_orphan_without_overwriting_it() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let source = create_fixture(&directory, "legacy-orphan.csv");
        let store = DatasetStore::open(&root).unwrap();
        let prepared = staged(&store, &source);
        let dataset_id = prepared.manifest.dataset_id.clone();
        let destination = store.datasets_directory.join(&dataset_id);
        promote_with_bounded_retry(&prepared.staging_directory, &destination).unwrap();
        drop(prepared);
        drop(store);

        let reopened = DatasetStore::open(&root).unwrap();
        let result = reopened
            .commit_prepared(staged(&reopened, &source))
            .unwrap();
        assert_eq!(result.disposition, CommitDisposition::Reused);
        assert!(destination.is_dir());
        let attempts = reopened.list_attempts().unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().any(|item| {
            item.diagnostic_code.as_deref() == Some("RECOVERED_UNJOURNALED_PROMOTION")
        }));
    }

    #[test]
    fn recovery_fails_closed_and_preserves_tampered_promoted_evidence() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let source = create_fixture(&directory, "tampered-promotion.csv");
        let store = DatasetStore::open(&root).unwrap();
        let prepared = staged(&store, &source);
        let dataset_id = prepared.manifest.dataset_id.clone();
        let stage_name = prepared
            .staging_directory
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let pending = PendingDatasetPromotion {
            staging_name: stage_name,
            dataset: metadata_from_prepared(&prepared),
            attempt: terminal_attempt(
                "IMPORTED",
                &prepared.manifest.provenance.source_file_name,
                None,
                None,
            ),
        };
        {
            let connection = store.connection_guard().unwrap();
            save_pending_dataset_promotion(&connection, &pending).unwrap();
        }
        let destination = store.datasets_directory.join(&dataset_id);
        promote_with_bounded_retry(&prepared.staging_directory, &destination).unwrap();
        let parquet = destination.join("bars.parquet");
        fs::write(&parquet, b"tampered evidence").unwrap();
        drop(store);

        let error = match DatasetStore::open(&root) {
            Ok(_) => panic!("tampered pending promotion must not be accepted"),
            Err(error) => error,
        };
        assert!(matches!(error, DatasetStoreError::RecoveryBlocked { .. }));
        assert_eq!(fs::read(&parquet).unwrap(), b"tampered evidence");
        let connection = open_database(root.join("research.sqlite3")).unwrap();
        assert_eq!(
            list_pending_dataset_promotions(&connection).unwrap().len(),
            1
        );
    }

    #[test]
    fn abandoned_unjournaled_stage_is_preserved_without_blocking_a_new_import() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let source = create_fixture(&directory, "abandoned-stage.csv");
        let store = DatasetStore::open(&root).unwrap();
        let abandoned = staged(&store, &source);
        let abandoned_path = abandoned.staging_directory.clone();
        drop(abandoned);
        drop(store);

        let reopened = DatasetStore::open(&root).unwrap();
        assert!(abandoned_path.is_dir());
        assert!(reopened.list_datasets().unwrap().is_empty());
        let result = reopened
            .commit_prepared(staged(&reopened, &source))
            .unwrap();
        assert_eq!(result.disposition, CommitDisposition::Imported);
        assert!(abandoned_path.is_dir());
    }

    #[test]
    fn identical_reimport_reuses_identity_and_records_a_terminal_attempt() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let source = create_fixture(&directory, "bars.csv");
        let first = store.commit_prepared(staged(&store, &source)).unwrap();
        let second = store.commit_prepared(staged(&store, &source)).unwrap();
        assert_eq!(first.dataset_id, second.dataset_id);
        assert_eq!(second.disposition, CommitDisposition::Reused);
        assert_eq!(store.list_datasets().unwrap().len(), 1);
        let statuses: Vec<_> = store
            .list_attempts()
            .unwrap()
            .into_iter()
            .map(|attempt| attempt.status)
            .collect();
        assert_eq!(statuses, vec!["REUSED", "IMPORTED"]);
    }

    #[test]
    fn catalog_verification_reports_each_checked_snapshot_incrementally() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let first_source = create_fixture(&directory, "catalog-first.csv");
        let second_source = directory.path().join("catalog-second.csv");
        fs::write(&second_source, FIXTURE.replace("1.1000", "1.1001")).unwrap();
        store
            .commit_prepared(staged(&store, &first_source))
            .unwrap();
        store
            .commit_prepared(staged(&store, &second_source))
            .unwrap();

        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();
        let entries = store
            .list_datasets_with_progress(&cancel, |completed, total, entry| {
                progress.push((completed, total, entry.dataset_id.clone()));
            })
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.integrity.is_valid()));
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0].0, 1);
        assert_eq!(progress[0].1, 2);
        assert_eq!(progress[1].0, 2);
        assert_eq!(progress[1].1, 2);
    }

    #[test]
    fn catalog_cancellation_interrupts_hashing_of_a_large_fixture_file() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let source = create_fixture(&directory, "large-catalog-fixture.csv");
        store.commit_prepared(staged(&store, &source)).unwrap();
        let dataset = store.list_datasets().unwrap().remove(0);
        let raw_path = store
            .datasets_directory
            .join(dataset.dataset_id)
            .join("raw-source.csv");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&raw_path)
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let cancellation = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            signal.store(true, Ordering::Relaxed);
        });
        let result = store.list_datasets_with_progress(&cancel, |_, _, _| {});
        cancellation.join().unwrap();
        assert!(matches!(
            result,
            Err(DatasetStoreError::Data(DataGroundError::Cancelled))
        ));
    }

    #[test]
    fn store_rejects_symlinked_staging_directory() {
        for directory_name in [".staging", "datasets"] {
            let directory = TempDir::new().unwrap();
            let root = directory.path().join("store");
            let outside = directory.path().join("outside");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&outside).unwrap();
            let linked_directory = root.join(directory_name);
            #[cfg(unix)]
            let link_result = std::os::unix::fs::symlink(&outside, &linked_directory);
            #[cfg(windows)]
            let link_result = std::os::windows::fs::symlink_dir(&outside, &linked_directory);
            link_result.expect("the test environment must allow local directory symlink fixtures");

            assert!(matches!(
                DatasetStore::open(&root),
                Err(DatasetStoreError::UnsafeStorePath)
            ));
            assert!(!outside.join("research.sqlite3").exists());
        }
    }

    #[test]
    fn store_rejects_symlinked_metadata_database() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let outside_database = directory.path().join("outside.sqlite3");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside_database, b"external fixture bytes").unwrap();
        let linked_database = root.join("research.sqlite3");
        #[cfg(unix)]
        let link_result = std::os::unix::fs::symlink(&outside_database, &linked_database);
        #[cfg(windows)]
        let link_result = std::os::windows::fs::symlink_file(&outside_database, &linked_database);
        link_result.expect("the test environment must allow local file symlink fixtures");

        assert!(matches!(
            DatasetStore::open(&root),
            Err(DatasetStoreError::UnsafeStorePath)
        ));
        assert_eq!(
            fs::read(outside_database).unwrap(),
            b"external fixture bytes"
        );
    }

    #[test]
    fn tampered_manifest_raw_or_parquet_is_quarantined_on_every_read() {
        for file_name in ["manifest.json", "raw-source.csv", "bars.parquet"] {
            let directory = TempDir::new().unwrap();
            let store = DatasetStore::open(directory.path().join("store")).unwrap();
            let source = create_fixture(&directory, "bars.csv");
            let result = store.commit_prepared(staged(&store, &source)).unwrap();
            let path = directory
                .path()
                .join("store")
                .join("datasets")
                .join(result.dataset_id)
                .join(file_name);
            fs::write(path, b"tampered").unwrap();
            let entry = store.list_datasets().unwrap().remove(0);
            assert_eq!(entry.status, DatasetStatus::Quarantined, "{file_name}");
            assert!(entry.diagnostic.is_some());
        }
    }

    #[test]
    fn failed_registry_commit_preserves_promotion_for_startup_recovery() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("store");
        let store = DatasetStore::open(&root).unwrap();
        let source = create_fixture(&directory, "bars.csv");
        let prepared = staged(&store, &source);
        let dataset_id = prepared.manifest.dataset_id.clone();
        let destination = directory
            .path()
            .join("store")
            .join("datasets")
            .join(&dataset_id);
        {
            let connection = store.connection_guard().unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER reject_dataset_insert BEFORE INSERT ON datasets
                     BEGIN SELECT RAISE(ABORT, 'fixture failure injection'); END;",
                )
                .unwrap();
        }
        assert!(store.commit_prepared(prepared).is_err());
        assert!(destination.is_dir());
        assert!(store.list_datasets().unwrap().is_empty());
        {
            let connection = store.connection_guard().unwrap();
            let pending = list_pending_dataset_promotions(&connection).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].dataset.dataset_id, dataset_id);
        }
        drop(store);

        let connection = open_database(root.join("research.sqlite3")).unwrap();
        connection
            .execute_batch("DROP TRIGGER reject_dataset_insert;")
            .unwrap();
        drop(connection);

        let recovered = DatasetStore::open(&root).unwrap();
        let datasets = recovered.list_datasets().unwrap();
        assert_eq!(datasets.len(), 1);
        assert_eq!(datasets[0].dataset_id, dataset_id);
        assert!(datasets[0].integrity.is_valid());
        assert!(
            list_pending_dataset_promotions(&recovered.connection_guard().unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(recovered.list_attempts().unwrap().iter().any(|attempt| {
            attempt.status == "IMPORTED" && attempt.source_file_name == "bars.csv"
        }));
    }

    #[test]
    fn corrupt_metadata_sha_is_not_rendered_as_a_valid_dataset() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let source = create_fixture(&directory, "bars.csv");
        let result = store.commit_prepared(staged(&store, &source)).unwrap();
        {
            let connection = store.connection_guard().unwrap();
            connection
                .execute_batch("DROP TRIGGER datasets_prevent_update;")
                .unwrap();
            connection
                .execute(
                    "UPDATE datasets SET manifest_sha256 = ?1 WHERE dataset_id = ?2",
                    rusqlite::params!["0".repeat(64), result.dataset_id],
                )
                .unwrap();
        }
        let entry = store.inspect_dataset(&result.dataset_id).unwrap().unwrap();
        assert_eq!(entry.status, DatasetStatus::Quarantined);
        assert!(!entry.integrity.manifest_valid);
    }
}
