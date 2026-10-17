use std::{
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Mutex, MutexGuard},
    thread,
    time::Duration,
};

use chrono::Utc;
use rg_persistence::{
    get_dataset_by_fingerprint, get_dataset_metadata, list_dataset_metadata, list_import_attempts,
    open_database, save_dataset_and_attempt, save_import_attempt, DatasetMetadata,
    ImportAttemptRecord, PersistenceError,
};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use thiserror::Error;
use uuid::Uuid;

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
        Ok(Self {
            staging_directory,
            datasets_directory,
            connection: Mutex::new(connection),
            operation_lock: Mutex::new(()),
        })
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
        let stage = prepared.staging_directory.clone();
        let source_name = prepared.manifest.provenance.source_file_name.clone();
        let result = self.commit_prepared_locked(&prepared);
        if let Err(error) = &result {
            let status = if matches!(error, DatasetStoreError::Data(DataGroundError::Cancelled)) {
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
        if stage.exists() {
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
        let mut connection = self.connection_guard()?;
        if let Some(existing) = get_dataset_by_fingerprint(&connection, &manifest.fingerprint)? {
            let entry = self.verify_metadata(&existing);
            if !entry.integrity.is_valid() || entry.status == DatasetStatus::Quarantined {
                return Err(DatasetStoreError::ExistingDatasetCorrupt);
            }
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
        if destination.exists() {
            return Err(DatasetStoreError::OrphanDirectoryExists);
        }
        promote_with_bounded_retry(&prepared.staging_directory, &destination)?;
        let metadata = metadata_from_prepared(prepared);
        let attempt = terminal_attempt(
            "IMPORTED",
            &manifest.provenance.source_file_name,
            None,
            None,
        );
        match save_dataset_and_attempt(&mut connection, &metadata, &attempt) {
            Ok(()) => Ok(ImportCommitResult {
                dataset_id: manifest.dataset_id.clone(),
                fingerprint: manifest.fingerprint.clone(),
                status: manifest.status.clone(),
                disposition: CommitDisposition::Imported,
            }),
            Err(error) => {
                let existing = get_dataset_by_fingerprint(&connection, &manifest.fingerprint)?;
                if let Some(existing) = existing {
                    let entry = self.verify_metadata(&existing);
                    if entry.integrity.is_valid() && entry.status != DatasetStatus::Quarantined {
                        save_import_attempt(
                            &connection,
                            &terminal_attempt(
                                "REUSED",
                                &manifest.provenance.source_file_name,
                                Some("CONCURRENT_IDENTICAL_IMPORT".into()),
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
                }
                let _ = fs::remove_dir_all(&destination);
                Err(error.into())
            }
        }
    }

    pub fn list_datasets(&self) -> Result<Vec<DatasetCatalogEntry>, DatasetStoreError> {
        let connection = self.connection_guard()?;
        let metadata = list_dataset_metadata(&connection)?;
        Ok(metadata
            .iter()
            .map(|item| self.verify_metadata(item))
            .collect())
    }

    pub fn inspect_dataset(
        &self,
        dataset_id: &str,
    ) -> Result<Option<DatasetCatalogEntry>, DatasetStoreError> {
        validate_dataset_id(dataset_id)?;
        let connection = self.connection_guard()?;
        let metadata = get_dataset_metadata(&connection, dataset_id)?;
        Ok(metadata.map(|item| self.verify_metadata(&item)))
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

    fn verify_metadata(&self, metadata: &DatasetMetadata) -> DatasetCatalogEntry {
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
        });
        let directory = self.datasets_directory.join(&metadata.dataset_id);
        let (manifest_file_valid, raw_source_valid, canonical_parquet_valid) =
            if validate_dataset_id(&metadata.dataset_id).is_ok() && directory_is_regular(&directory)
            {
                let manifest_valid =
                    read_verified_file(&directory.join("manifest.json"), &metadata.manifest_sha256);
                let raw_name = manifest
                    .as_ref()
                    .map(|value| format!("raw-source.{}", value.provenance.source_format));
                let raw_valid = raw_name.as_deref().is_some_and(|name| {
                    read_verified_file(&directory.join(name), &metadata.raw_sha256)
                });
                let parquet_valid =
                    read_verified_file(&directory.join("bars.parquet"), &metadata.parquet_sha256);
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
        DatasetCatalogEntry {
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
        }
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
    let raw_valid = read_verified_file(
        &canonical_stage.join(raw_name),
        &prepared.manifest.provenance.raw_sha256,
    );
    let parquet_valid = read_verified_file(
        &canonical_stage.join("bars.parquet"),
        &prepared.manifest.canonical_bars_sha256,
    );
    let manifest_valid = read_verified_file(
        &canonical_stage.join("manifest.json"),
        &prepared.manifest_sha256,
    );
    if !raw_valid || !parquet_valid || !manifest_valid {
        return Err(DatasetStoreError::InvalidPrepared(
            "staged manifest, source copy, or Parquet snapshot digest does not match".into(),
        ));
    }
    Ok(())
}

fn promote_with_bounded_retry(source: &Path, destination: &Path) -> Result<(), DatasetStoreError> {
    let mut last_error = None;
    for attempt in 0..3 {
        if destination.exists() {
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

fn read_verified_file(path: &Path, expected_sha256: &str) -> bool {
    if !is_sha256(expected_sha256) {
        return false;
    }
    if !fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    {
        return false;
    }
    let cancel = AtomicBool::new(false);
    hash_file(path, &cancel).is_ok_and(|actual| actual == expected_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CalendarPolicy, CsvDelimiter, FieldMapping, MarketRole, ResourceQuotas, TimestampSemantics,
    };
    use std::{collections::BTreeMap, fs};
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
    fn failed_commit_removes_promoted_bytes_and_records_failure() {
        let directory = TempDir::new().unwrap();
        let store = DatasetStore::open(directory.path().join("store")).unwrap();
        let source = create_fixture(&directory, "bars.csv");
        let prepared = staged(&store, &source);
        let destination = directory
            .path()
            .join("store")
            .join("datasets")
            .join(&prepared.manifest.dataset_id);
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
        assert!(!destination.exists());
        assert!(store.list_datasets().unwrap().is_empty());
        let attempts = store.list_attempts().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "FAILED");
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
