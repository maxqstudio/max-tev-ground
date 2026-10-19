use std::path::Path;

use rg_contracts::ExperimentManifest;
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 5;

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("manifest contract error: {0}")]
    Contract(#[from] rg_contracts::ContractError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: u32, supported: u32 },
    #[error("migration sequence is invalid at version {0}")]
    InvalidMigrationSequence(u32),
    #[error("migration checksum changed for version {0}")]
    MigrationChecksumMismatch(u32),
    #[error("experiment ID already exists: {0}")]
    ExperimentAlreadyExists(String),
    #[error("dataset ID or fingerprint already exists: {0}")]
    DatasetAlreadyExists(String),
    #[error("dataset metadata is invalid: {0}")]
    InvalidDatasetMetadata(String),
    #[error("stored manifest SHA-256 does not match manifest bytes for experiment: {0}")]
    ManifestChecksumMismatch(String),
}

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "experiment_metadata_foundation",
        sql: "
        CREATE TABLE experiments (
            experiment_id TEXT PRIMARY KEY NOT NULL,
            schema_version INTEGER NOT NULL CHECK (schema_version > 0),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            source_revision TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('CREATED', 'FAILED', 'COMPLETED'))
        );
        CREATE INDEX experiments_created_at_idx
            ON experiments(created_at_utc DESC, experiment_id);
    ",
    },
    Migration {
        version: 2,
        name: "immutable_experiment_metadata_triggers",
        sql: "
        CREATE TRIGGER experiments_prevent_update
        BEFORE UPDATE ON experiments
        BEGIN
            SELECT RAISE(ABORT, 'experiment metadata is immutable');
        END;
        CREATE TRIGGER experiments_prevent_delete
        BEFORE DELETE ON experiments
        BEGIN
            SELECT RAISE(ABORT, 'experiment metadata is immutable');
        END;
    ",
    },
    Migration {
        version: 3,
        name: "immutable_dataset_registry_and_import_attempts",
        sql: "
        CREATE TABLE datasets (
            dataset_id TEXT PRIMARY KEY NOT NULL,
            fingerprint TEXT NOT NULL UNIQUE CHECK (length(fingerprint) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            parquet_sha256 TEXT NOT NULL CHECK (length(parquet_sha256) = 64),
            raw_sha256 TEXT NOT NULL CHECK (length(raw_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('READY', 'BLOCKED', 'QUARANTINED'))
        );
        CREATE INDEX datasets_created_at_idx
            ON datasets(created_at_utc DESC, dataset_id);
        CREATE TRIGGER datasets_prevent_update
        BEFORE UPDATE ON datasets
        BEGIN
            SELECT RAISE(ABORT, 'dataset metadata is immutable');
        END;
        CREATE TRIGGER datasets_prevent_delete
        BEFORE DELETE ON datasets
        BEGIN
            SELECT RAISE(ABORT, 'dataset metadata is immutable');
        END;
        CREATE TABLE import_attempts (
            attempt_id TEXT PRIMARY KEY NOT NULL,
            started_at_utc TEXT NOT NULL,
            completed_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('IMPORTED', 'FAILED', 'CANCELLED')),
            source_file_name TEXT NOT NULL,
            diagnostic_code TEXT,
            diagnostic_message TEXT
        );
        CREATE INDEX import_attempts_completed_idx
            ON import_attempts(completed_at_utc DESC, attempt_id);
        CREATE TRIGGER import_attempts_prevent_update
        BEFORE UPDATE ON import_attempts
        BEGIN
            SELECT RAISE(ABORT, 'import attempt records are immutable');
        END;
        CREATE TRIGGER import_attempts_prevent_delete
        BEFORE DELETE ON import_attempts
        BEGIN
            SELECT RAISE(ABORT, 'import attempt records are immutable');
        END;
    ",
    },
    Migration {
        version: 4,
        name: "idempotent_import_attempts",
        sql: "
        DROP TRIGGER import_attempts_prevent_update;
        DROP TRIGGER import_attempts_prevent_delete;
        DROP INDEX import_attempts_completed_idx;
        ALTER TABLE import_attempts RENAME TO import_attempts_v3;
        CREATE TABLE import_attempts (
            attempt_id TEXT PRIMARY KEY NOT NULL,
            started_at_utc TEXT NOT NULL,
            completed_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('IMPORTED', 'REUSED', 'FAILED', 'CANCELLED')),
            source_file_name TEXT NOT NULL,
            diagnostic_code TEXT,
            diagnostic_message TEXT
        );
        INSERT INTO import_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
            diagnostic_code, diagnostic_message
        ) SELECT attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
                 diagnostic_code, diagnostic_message FROM import_attempts_v3;
        DROP TABLE import_attempts_v3;
        CREATE INDEX import_attempts_completed_idx
            ON import_attempts(completed_at_utc DESC, attempt_id);
        CREATE TRIGGER import_attempts_prevent_update
        BEFORE UPDATE ON import_attempts
        BEGIN
            SELECT RAISE(ABORT, 'import attempt records are immutable');
        END;
        CREATE TRIGGER import_attempts_prevent_delete
        BEFORE DELETE ON import_attempts
        BEGIN
            SELECT RAISE(ABORT, 'import attempt records are immutable');
        END;
    ",
    },
    Migration {
        version: 5,
        name: "crash_recoverable_dataset_promotions",
        sql: "
        CREATE TABLE dataset_promotions (
            dataset_id TEXT PRIMARY KEY NOT NULL,
            staging_name TEXT NOT NULL,
            fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            parquet_sha256 TEXT NOT NULL CHECK (length(parquet_sha256) = 64),
            raw_sha256 TEXT NOT NULL CHECK (length(raw_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('READY', 'BLOCKED', 'QUARANTINED')),
            attempt_id TEXT NOT NULL UNIQUE,
            attempt_started_at_utc TEXT NOT NULL,
            attempt_completed_at_utc TEXT NOT NULL,
            source_file_name TEXT NOT NULL,
            diagnostic_code TEXT,
            diagnostic_message TEXT
        );
        CREATE INDEX dataset_promotions_staging_idx ON dataset_promotions(staging_name);
    ",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredExperiment {
    pub experiment_id: String,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub created_at_utc: String,
    pub source_revision: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasetMetadata {
    pub dataset_id: String,
    pub fingerprint: String,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub parquet_sha256: String,
    pub raw_sha256: String,
    pub created_at_utc: String,
    pub status: String,
    pub manifest_integrity_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDatasetPromotion {
    pub staging_name: String,
    pub dataset: DatasetMetadata,
    pub attempt: ImportAttemptRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ImportAttemptRecord {
    pub attempt_id: String,
    pub started_at_utc: String,
    pub completed_at_utc: String,
    pub status: String,
    pub source_file_name: String,
    pub diagnostic_code: Option<String>,
    pub diagnostic_message: Option<String>,
}

pub fn open_database(path: impl AsRef<Path>) -> Result<Connection, PersistenceError> {
    let mut connection = Connection::open(path)?;
    configure(&connection)?;
    migrate(&mut connection)?;
    Ok(connection)
}

pub fn configure(connection: &Connection) -> Result<(), PersistenceError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

pub fn schema_version(connection: &Connection) -> Result<u32, PersistenceError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

pub fn migrate(connection: &mut Connection) -> Result<u32, PersistenceError> {
    migrate_with(connection, MIGRATIONS, CURRENT_SCHEMA_VERSION)
}

fn migrate_with(
    connection: &mut Connection,
    migrations: &[Migration],
    supported_version: u32,
) -> Result<u32, PersistenceError> {
    let mut current = schema_version(connection)?;
    if current > supported_version {
        return Err(PersistenceError::FutureSchema {
            found: current,
            supported: supported_version,
        });
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY NOT NULL,
            name TEXT NOT NULL,
            checksum TEXT NOT NULL,
            applied_at_utc TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
    )?;

    for migration in migrations {
        if migration.version <= current {
            let stored_checksum: String = connection.query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                [migration.version],
                |row| row.get(0),
            )?;
            if stored_checksum != checksum(migration.sql) {
                return Err(PersistenceError::MigrationChecksumMismatch(
                    migration.version,
                ));
            }
            continue;
        }
        let expected = current + 1;
        if migration.version != expected {
            return Err(PersistenceError::InvalidMigrationSequence(expected));
        }

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, checksum) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, checksum(migration.sql)],
        )?;
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
        current = migration.version;
    }
    Ok(current)
}

pub fn save_experiment_metadata(
    connection: &Connection,
    manifest: &ExperimentManifest,
    created_at_utc: &str,
    source_revision: &str,
) -> Result<StoredExperiment, PersistenceError> {
    manifest.validate()?;
    if created_at_utc.trim().is_empty() || source_revision.trim().is_empty() {
        return Err(rusqlite::Error::InvalidParameterName(
            "created_at_utc and source_revision are required".into(),
        )
        .into());
    }
    let manifest_json = manifest.canonical_json()?;
    let manifest_sha256 = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes())
        .as_str()
        .to_owned();
    let stored = StoredExperiment {
        experiment_id: manifest.experiment_id.to_string(),
        manifest_json,
        manifest_sha256,
        created_at_utc: created_at_utc.to_owned(),
        source_revision: source_revision.to_owned(),
        status: "CREATED".into(),
    };
    let result = connection.execute(
        "INSERT INTO experiments(
            experiment_id, schema_version, manifest_json, manifest_sha256,
            created_at_utc, source_revision, status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            stored.experiment_id,
            manifest.schema_version,
            stored.manifest_json,
            stored.manifest_sha256,
            stored.created_at_utc,
            stored.source_revision,
            stored.status
        ],
    );
    match result {
        Ok(_) => Ok(stored),
        Err(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
        {
            Err(PersistenceError::ExperimentAlreadyExists(
                manifest.experiment_id.to_string(),
            ))
        }
        Err(error) => Err(error.into()),
    }
}

pub fn get_experiment_metadata(
    connection: &Connection,
    experiment_id: &str,
) -> Result<Option<StoredExperiment>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT experiment_id, manifest_json, manifest_sha256, created_at_utc,
                source_revision, status
         FROM experiments WHERE experiment_id = ?1",
    )?;
    let mut rows = statement.query([experiment_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let stored = StoredExperiment {
        experiment_id: row.get(0)?,
        manifest_json: row.get(1)?,
        manifest_sha256: row.get(2)?,
        created_at_utc: row.get(3)?,
        source_revision: row.get(4)?,
        status: row.get(5)?,
    };
    let expected_sha256 = rg_contracts::Sha256Digest::of_bytes(stored.manifest_json.as_bytes());
    if stored.manifest_sha256 != expected_sha256.as_str() {
        return Err(PersistenceError::ManifestChecksumMismatch(
            stored.experiment_id,
        ));
    }
    Ok(Some(stored))
}

pub fn save_dataset_and_attempt(
    connection: &mut Connection,
    dataset: &DatasetMetadata,
    attempt: &ImportAttemptRecord,
) -> Result<(), PersistenceError> {
    validate_dataset_metadata(dataset)?;
    if attempt.attempt_id.trim().is_empty()
        || attempt.started_at_utc.trim().is_empty()
        || attempt.completed_at_utc.trim().is_empty()
        || attempt.source_file_name.trim().is_empty()
        || attempt.status != "IMPORTED"
    {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "successful imports require immutable terminal attempt metadata".into(),
        ));
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = tx.execute(
        "INSERT INTO datasets (
            dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
            raw_sha256, created_at_utc, status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            dataset.dataset_id,
            dataset.fingerprint,
            dataset.manifest_json,
            dataset.manifest_sha256,
            dataset.parquet_sha256,
            dataset.raw_sha256,
            dataset.created_at_utc,
            dataset.status
        ],
    );
    match result {
        Ok(_) => {}
        Err(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
        {
            return Err(PersistenceError::DatasetAlreadyExists(
                dataset.dataset_id.clone(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    tx.execute(
        "INSERT INTO import_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            attempt.attempt_id,
            attempt.started_at_utc,
            attempt.completed_at_utc,
            attempt.status,
            attempt.source_file_name,
            attempt.diagnostic_code,
            attempt.diagnostic_message
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn save_pending_dataset_promotion(
    connection: &Connection,
    pending: &PendingDatasetPromotion,
) -> Result<(), PersistenceError> {
    validate_dataset_metadata(&pending.dataset)?;
    validate_imported_attempt(&pending.attempt)?;
    if pending.staging_name.trim().is_empty()
        || pending.staging_name.contains(['/', '\\'])
        || pending.staging_name == "."
        || pending.staging_name == ".."
    {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "promotion staging identity is invalid".into(),
        ));
    }
    connection.execute(
        "INSERT INTO dataset_promotions (
            dataset_id, staging_name, fingerprint, manifest_json, manifest_sha256,
            parquet_sha256, raw_sha256, created_at_utc, status, attempt_id,
            attempt_started_at_utc, attempt_completed_at_utc, source_file_name,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            pending.dataset.dataset_id,
            pending.staging_name,
            pending.dataset.fingerprint,
            pending.dataset.manifest_json,
            pending.dataset.manifest_sha256,
            pending.dataset.parquet_sha256,
            pending.dataset.raw_sha256,
            pending.dataset.created_at_utc,
            pending.dataset.status,
            pending.attempt.attempt_id,
            pending.attempt.started_at_utc,
            pending.attempt.completed_at_utc,
            pending.attempt.source_file_name,
            pending.attempt.diagnostic_code,
            pending.attempt.diagnostic_message,
        ],
    )?;
    Ok(())
}

pub fn list_pending_dataset_promotions(
    connection: &Connection,
) -> Result<Vec<PendingDatasetPromotion>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT staging_name, dataset_id, fingerprint, manifest_json, manifest_sha256,
                parquet_sha256, raw_sha256, created_at_utc, status, attempt_id,
                attempt_started_at_utc, attempt_completed_at_utc, source_file_name,
                diagnostic_code, diagnostic_message
         FROM dataset_promotions ORDER BY dataset_id",
    )?;
    let rows = statement.query_map([], |row| {
        let manifest_json: String = row.get(3)?;
        let manifest_sha256: String = row.get(4)?;
        let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
        Ok(PendingDatasetPromotion {
            staging_name: row.get(0)?,
            dataset: DatasetMetadata {
                dataset_id: row.get(1)?,
                fingerprint: row.get(2)?,
                manifest_json,
                manifest_integrity_valid: manifest_sha256 == actual.as_str(),
                manifest_sha256,
                parquet_sha256: row.get(5)?,
                raw_sha256: row.get(6)?,
                created_at_utc: row.get(7)?,
                status: row.get(8)?,
            },
            attempt: ImportAttemptRecord {
                attempt_id: row.get(9)?,
                started_at_utc: row.get(10)?,
                completed_at_utc: row.get(11)?,
                status: "IMPORTED".into(),
                source_file_name: row.get(12)?,
                diagnostic_code: row.get(13)?,
                diagnostic_message: row.get(14)?,
            },
        })
    })?;
    let pending = rows.collect::<Result<Vec<_>, _>>()?;
    for item in &pending {
        validate_dataset_metadata(&item.dataset)?;
        validate_imported_attempt(&item.attempt)?;
        if item.staging_name.trim().is_empty()
            || item.staging_name.contains(['/', '\\'])
            || item.staging_name == "."
            || item.staging_name == ".."
        {
            return Err(PersistenceError::InvalidDatasetMetadata(
                "stored promotion staging identity is invalid".into(),
            ));
        }
    }
    Ok(pending)
}

pub fn clear_pending_dataset_promotion(
    connection: &Connection,
    dataset_id: &str,
) -> Result<bool, PersistenceError> {
    Ok(connection.execute(
        "DELETE FROM dataset_promotions WHERE dataset_id = ?1",
        [dataset_id],
    )? == 1)
}

pub fn complete_pending_dataset_promotion(
    connection: &mut Connection,
    dataset_id: &str,
) -> Result<(), PersistenceError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending = {
        let mut statement = tx.prepare(
            "SELECT staging_name, dataset_id, fingerprint, manifest_json, manifest_sha256,
                    parquet_sha256, raw_sha256, created_at_utc, status, attempt_id,
                    attempt_started_at_utc, attempt_completed_at_utc, source_file_name,
                    diagnostic_code, diagnostic_message
             FROM dataset_promotions WHERE dataset_id = ?1",
        )?;
        let mut rows = statement.query([dataset_id])?;
        rows.next()?
            .map(|row| {
                let manifest_json: String = row.get(3)?;
                let manifest_sha256: String = row.get(4)?;
                let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
                Ok::<PendingDatasetPromotion, rusqlite::Error>(PendingDatasetPromotion {
                    staging_name: row.get(0)?,
                    dataset: DatasetMetadata {
                        dataset_id: row.get(1)?,
                        fingerprint: row.get(2)?,
                        manifest_integrity_valid: manifest_sha256 == actual.as_str(),
                        manifest_json,
                        manifest_sha256,
                        parquet_sha256: row.get(5)?,
                        raw_sha256: row.get(6)?,
                        created_at_utc: row.get(7)?,
                        status: row.get(8)?,
                    },
                    attempt: ImportAttemptRecord {
                        attempt_id: row.get(9)?,
                        started_at_utc: row.get(10)?,
                        completed_at_utc: row.get(11)?,
                        status: "IMPORTED".into(),
                        source_file_name: row.get(12)?,
                        diagnostic_code: row.get(13)?,
                        diagnostic_message: row.get(14)?,
                    },
                })
            })
            .transpose()?
    }
    .ok_or_else(|| {
        PersistenceError::InvalidDatasetMetadata(format!(
            "pending promotion is missing for dataset {dataset_id}"
        ))
    })?;
    validate_dataset_metadata(&pending.dataset)?;
    validate_imported_attempt(&pending.attempt)?;
    tx.execute(
        "INSERT INTO datasets (
            dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
            raw_sha256, created_at_utc, status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            pending.dataset.dataset_id,
            pending.dataset.fingerprint,
            pending.dataset.manifest_json,
            pending.dataset.manifest_sha256,
            pending.dataset.parquet_sha256,
            pending.dataset.raw_sha256,
            pending.dataset.created_at_utc,
            pending.dataset.status,
        ],
    )?;
    tx.execute(
        "INSERT INTO import_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            pending.attempt.attempt_id,
            pending.attempt.started_at_utc,
            pending.attempt.completed_at_utc,
            pending.attempt.status,
            pending.attempt.source_file_name,
            pending.attempt.diagnostic_code,
            pending.attempt.diagnostic_message,
        ],
    )?;
    tx.execute(
        "DELETE FROM dataset_promotions WHERE dataset_id = ?1",
        [dataset_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn validate_imported_attempt(attempt: &ImportAttemptRecord) -> Result<(), PersistenceError> {
    if attempt.attempt_id.trim().is_empty()
        || attempt.started_at_utc.trim().is_empty()
        || attempt.completed_at_utc.trim().is_empty()
        || attempt.source_file_name.trim().is_empty()
        || attempt.status != "IMPORTED"
    {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "pending promotion requires immutable imported attempt metadata".into(),
        ));
    }
    Ok(())
}

pub fn save_import_attempt(
    connection: &Connection,
    attempt: &ImportAttemptRecord,
) -> Result<(), PersistenceError> {
    if attempt.attempt_id.trim().is_empty()
        || attempt.started_at_utc.trim().is_empty()
        || attempt.completed_at_utc.trim().is_empty()
        || attempt.source_file_name.trim().is_empty()
        || !matches!(attempt.status.as_str(), "REUSED" | "FAILED" | "CANCELLED")
    {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "failed or cancelled attempt metadata is invalid".into(),
        ));
    }
    connection.execute(
        "INSERT INTO import_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            attempt.attempt_id,
            attempt.started_at_utc,
            attempt.completed_at_utc,
            attempt.status,
            attempt.source_file_name,
            attempt.diagnostic_code,
            attempt.diagnostic_message
        ],
    )?;
    Ok(())
}

pub fn get_dataset_metadata(
    connection: &Connection,
    dataset_id: &str,
) -> Result<Option<DatasetMetadata>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
                raw_sha256, created_at_utc, status
         FROM datasets WHERE dataset_id = ?1",
    )?;
    let mut rows = statement.query([dataset_id])?;
    Ok(rows.next()?.map(|row| dataset_from_row(row)).transpose()?)
}

pub fn get_dataset_by_fingerprint(
    connection: &Connection,
    fingerprint: &str,
) -> Result<Option<DatasetMetadata>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
                raw_sha256, created_at_utc, status
         FROM datasets WHERE fingerprint = ?1",
    )?;
    let mut rows = statement.query([fingerprint])?;
    Ok(rows.next()?.map(|row| dataset_from_row(row)).transpose()?)
}

pub fn list_dataset_metadata(
    connection: &Connection,
) -> Result<Vec<DatasetMetadata>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
                raw_sha256, created_at_utc, status
         FROM datasets ORDER BY created_at_utc DESC, dataset_id",
    )?;
    let rows = statement.query_map([], dataset_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn list_dataset_metadata_after(
    connection: &Connection,
    after_dataset_id: &str,
    limit: u64,
) -> Result<Vec<DatasetMetadata>, PersistenceError> {
    let limit = i64::try_from(limit).map_err(|_| {
        PersistenceError::InvalidDatasetMetadata("dataset metadata page size is too large".into())
    })?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT dataset_id, fingerprint, manifest_json, manifest_sha256, parquet_sha256,
                raw_sha256, created_at_utc, status
         FROM datasets WHERE dataset_id > ?1 ORDER BY dataset_id LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![after_dataset_id, limit], dataset_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn list_import_attempts(
    connection: &Connection,
) -> Result<Vec<ImportAttemptRecord>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT attempt_id, started_at_utc, completed_at_utc, status, source_file_name,
                diagnostic_code, diagnostic_message
         FROM import_attempts ORDER BY completed_at_utc DESC, attempt_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ImportAttemptRecord {
            attempt_id: row.get(0)?,
            started_at_utc: row.get(1)?,
            completed_at_utc: row.get(2)?,
            status: row.get(3)?,
            source_file_name: row.get(4)?,
            diagnostic_code: row.get(5)?,
            diagnostic_message: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn dataset_from_row(row: &rusqlite::Row<'_>) -> Result<DatasetMetadata, rusqlite::Error> {
    let manifest_json: String = row.get(2)?;
    let manifest_sha256: String = row.get(3)?;
    let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
    Ok(DatasetMetadata {
        dataset_id: row.get(0)?,
        fingerprint: row.get(1)?,
        manifest_json,
        manifest_integrity_valid: manifest_sha256 == actual.as_str(),
        manifest_sha256,
        parquet_sha256: row.get(4)?,
        raw_sha256: row.get(5)?,
        created_at_utc: row.get(6)?,
        status: row.get(7)?,
    })
}

fn validate_dataset_metadata(dataset: &DatasetMetadata) -> Result<(), PersistenceError> {
    let valid_sha =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if dataset.dataset_id.trim().is_empty()
        || !valid_sha(&dataset.fingerprint)
        || !valid_sha(&dataset.manifest_sha256)
        || !valid_sha(&dataset.parquet_sha256)
        || !valid_sha(&dataset.raw_sha256)
        || dataset.created_at_utc.trim().is_empty()
        || !matches!(dataset.status.as_str(), "READY" | "BLOCKED" | "QUARANTINED")
    {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "identifier, digest, timestamp, or status is invalid".into(),
        ));
    }
    let actual = rg_contracts::Sha256Digest::of_bytes(dataset.manifest_json.as_bytes());
    if dataset.manifest_sha256 != actual.as_str() {
        return Err(PersistenceError::InvalidDatasetMetadata(
            "manifest SHA-256 does not match manifest bytes".into(),
        ));
    }
    Ok(())
}

fn checksum(value: &str) -> String {
    rg_contracts::Sha256Digest::of_bytes(value.as_bytes())
        .as_str()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rg_contracts::{
        CandleTimestampContract, DatasetId, DatasetIdentity, DecisionAvailability,
        ExecutionSimulatorConfiguration, ExecutionSimulatorId, ExperimentId, FeatureEngineId,
        FeatureEngineIdentity, MissingDataPolicy, OosExposure, RiskConfiguration, Sha256Digest,
        StrategyId, StrategyIdentity, TemporalSplitConfiguration, CONTRACT_SCHEMA_VERSION,
    };

    fn manifest() -> ExperimentManifest {
        ExperimentManifest {
            schema_version: CONTRACT_SCHEMA_VERSION,
            experiment_id: ExperimentId::new(),
            dataset: DatasetIdentity {
                dataset_id: DatasetId::parse("fixture-eurusd-h1").unwrap(),
                fingerprint: Sha256Digest::of_bytes(b"fixture"),
                source_id: "fixture:persistence-test".into(),
                symbol: "EURUSD.m".into(),
                timeframe: "H1".into(),
                timestamps: CandleTimestampContract {
                    timezone: "UTC".into(),
                    open_at_field: "open_at".into(),
                    close_at_field: "close_at".into(),
                    decision_available_at_field: "available_at".into(),
                    decision_rule: DecisionAvailability::AtCandleClose,
                },
                missing_data: MissingDataPolicy::PreserveGaps,
            },
            feature_engine: FeatureEngineIdentity {
                engine_id: FeatureEngineId::parse("native").unwrap(),
                version: "1".into(),
                feature_set: vec![],
            },
            strategy: StrategyIdentity {
                strategy_id: StrategyId::parse("breakout").unwrap(),
                version: "1".into(),
                parameters: Default::default(),
            },
            model: None,
            policy: None,
            risk: RiskConfiguration {
                risk_per_trade_bps: 100,
            },
            execution: ExecutionSimulatorConfiguration {
                simulator_id: ExecutionSimulatorId::parse("bar-sim").unwrap(),
                version: "1".into(),
                spread_model_id: "fixed".into(),
                commission_bps: 1,
                slippage_ticks: 0,
            },
            temporal_split: TemporalSplitConfiguration {
                discovery_start_utc: "2024-01-01T00:00:00Z".into(),
                discovery_end_utc: "2024-02-01T00:00:00Z".into(),
                tuning_end_utc: "2024-03-01T00:00:00Z".into(),
                untouched_oos_start_utc: "2024-04-01T00:00:00Z".into(),
                untouched_oos_end_utc: "2024-05-01T00:00:00Z".into(),
                oos_exposure: OosExposure::Unexposed,
            },
            research_code_version: "git:test".into(),
            random_seed: Some(41),
        }
    }

    #[test]
    fn migration_sets_schema_version_and_is_idempotent() {
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(schema_version(&connection).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        let count: u32 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn version_three_registry_upgrades_import_attempts_without_losing_history() {
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(
            migrate_with(&mut connection, &MIGRATIONS[..3], 3).unwrap(),
            3
        );
        connection
            .execute(
                "INSERT INTO import_attempts (
                    attempt_id, started_at_utc, completed_at_utc, status, source_file_name
                 ) VALUES ('old-attempt', '2024-01-01T00:00:00Z', '2024-01-01T00:00:01Z', 'FAILED', 'fixture.csv')",
                [],
            )
            .unwrap();

        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        let attempts = list_import_attempts(&connection).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].attempt_id, "old-attempt");
        assert_eq!(attempts[0].status, "FAILED");

        save_import_attempt(
            &connection,
            &ImportAttemptRecord {
                attempt_id: "reuse-attempt".into(),
                started_at_utc: "2024-01-02T00:00:00Z".into(),
                completed_at_utc: "2024-01-02T00:00:00Z".into(),
                status: "REUSED".into(),
                source_file_name: "fixture.csv".into(),
                diagnostic_code: Some("IDENTICAL_FINGERPRINT".into()),
                diagnostic_message: None,
            },
        )
        .unwrap();
        assert!(connection
            .execute(
                "UPDATE import_attempts SET status = 'FAILED' WHERE attempt_id = 'reuse-attempt'",
                [],
            )
            .is_err());
    }

    #[test]
    fn version_one_database_upgrades_without_changing_its_migration_checksum() {
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(
            migrate_with(&mut connection, &MIGRATIONS[..1], 1).unwrap(),
            1
        );
        let checksum_before: String = connection
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        let checksum_after: String = connection
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checksum_before, checksum_after);
        assert_eq!(schema_version(&connection).unwrap(), CURRENT_SCHEMA_VERSION);
        let trigger_count: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'experiments'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_count, 2);
    }

    #[test]
    fn failed_migration_rolls_back_ddl_version_and_ledger_entry() {
        let mut connection = Connection::open_in_memory().unwrap();
        let broken = [Migration {
            version: 1,
            name: "intentional_failure_fixture",
            sql: "CREATE TABLE must_rollback(id INTEGER); INSERT INTO missing_table VALUES (1);",
        }];
        assert!(migrate_with(&mut connection, &broken, 1).is_err());

        let exists: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='must_rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0);
        assert_eq!(schema_version(&connection).unwrap(), 0);
        let applied: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn file_database_reopens_with_the_same_schema_and_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("research.sqlite3");
        let manifest = manifest();
        {
            let connection = open_database(&path).unwrap();
            save_experiment_metadata(
                &connection,
                &manifest,
                "2026-10-17T00:00:00Z",
                "fixture-revision",
            )
            .unwrap();
        }
        {
            let connection = open_database(&path).unwrap();
            assert_eq!(schema_version(&connection).unwrap(), CURRENT_SCHEMA_VERSION);
            let saved = get_experiment_metadata(&connection, manifest.experiment_id.as_str())
                .unwrap()
                .unwrap();
            assert_eq!(saved.manifest_json, manifest.canonical_json().unwrap());
            let migrations: u32 = connection
                .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(migrations, 5);
        }
    }
    #[test]
    fn metadata_insert_is_immutable_and_auditable() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let manifest = manifest();
        let stored = save_experiment_metadata(
            &connection,
            &manifest,
            "2026-10-17T00:00:00Z",
            "fixture-revision",
        )
        .unwrap();
        assert_eq!(stored.manifest_sha256.len(), 64);
        assert_eq!(stored.status, "CREATED");
        assert_eq!(
            get_experiment_metadata(&connection, manifest.experiment_id.as_str()).unwrap(),
            Some(stored.clone())
        );
        assert!(matches!(
            save_experiment_metadata(
                &connection,
                &manifest,
                "2026-10-18T00:00:00Z",
                "different-revision"
            ),
            Err(PersistenceError::ExperimentAlreadyExists(_))
        ));
        assert_eq!(
            get_experiment_metadata(&connection, manifest.experiment_id.as_str())
                .unwrap()
                .unwrap(),
            stored
        );
    }

    #[test]
    fn sqlite_rejects_direct_update_and_delete_of_experiment_metadata() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let manifest = manifest();
        let stored = save_experiment_metadata(
            &connection,
            &manifest,
            "2026-10-17T00:00:00Z",
            "fixture-revision",
        )
        .unwrap();

        assert!(connection
            .execute(
                "UPDATE experiments SET status = 'FAILED' WHERE experiment_id = ?1",
                [manifest.experiment_id.as_str()],
            )
            .is_err());
        assert!(connection
            .execute(
                "DELETE FROM experiments WHERE experiment_id = ?1",
                [manifest.experiment_id.as_str()],
            )
            .is_err());
        assert_eq!(
            get_experiment_metadata(&connection, manifest.experiment_id.as_str()).unwrap(),
            Some(stored)
        );
    }

    #[test]
    fn metadata_read_detects_manifest_tampering() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let manifest = manifest();
        save_experiment_metadata(
            &connection,
            &manifest,
            "2026-10-17T00:00:00Z",
            "fixture-revision",
        )
        .unwrap();

        // Simulate an out-of-band database file edit that bypassed the installed trigger.
        connection
            .execute_batch(
                "DROP TRIGGER experiments_prevent_update;
                 UPDATE experiments SET manifest_json = manifest_json || ' ';",
            )
            .unwrap();

        assert!(matches!(
            get_experiment_metadata(&connection, manifest.experiment_id.as_str()),
            Err(PersistenceError::ManifestChecksumMismatch(_))
        ));
    }

    #[test]
    fn failed_second_migration_rolls_back_its_ddl_version_and_ledger_entry() {
        let mut connection = Connection::open_in_memory().unwrap();
        let broken = [
            MIGRATIONS[0],
            Migration {
                version: 2,
                name: "intentional_failure_fixture",
                sql: "CREATE TRIGGER must_rollback BEFORE UPDATE ON experiments BEGIN SELECT RAISE(ABORT, 'blocked'); END; INSERT INTO missing_table VALUES (1);",
            },
        ];
        assert!(migrate_with(&mut connection, &broken, 2).is_err());

        assert_eq!(schema_version(&connection).unwrap(), 1);
        let applied: u32 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(applied, 1);
        let trigger_exists: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='must_rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger_exists, 0);
    }

    fn dataset_metadata() -> DatasetMetadata {
        let manifest_json = "{\"schema_version\":1,\"fixture\":true}".to_owned();
        DatasetMetadata {
            dataset_id: "ds_fixture".into(),
            fingerprint: "a".repeat(64),
            manifest_sha256: checksum(&manifest_json),
            manifest_json,
            parquet_sha256: "b".repeat(64),
            raw_sha256: "c".repeat(64),
            created_at_utc: "2026-10-17T00:00:00Z".into(),
            status: "READY".into(),
            manifest_integrity_valid: true,
        }
    }

    fn imported_attempt() -> ImportAttemptRecord {
        ImportAttemptRecord {
            attempt_id: "attempt_fixture".into(),
            started_at_utc: "2026-10-17T00:00:00Z".into(),
            completed_at_utc: "2026-10-17T00:00:01Z".into(),
            status: "IMPORTED".into(),
            source_file_name: "fixture.csv".into(),
            diagnostic_code: None,
            diagnostic_message: None,
        }
    }

    #[test]
    fn dataset_and_attempt_commit_together_and_are_immutable() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let dataset = dataset_metadata();
        let attempt = imported_attempt();
        save_dataset_and_attempt(&mut connection, &dataset, &attempt).unwrap();
        assert_eq!(
            get_dataset_metadata(&connection, &dataset.dataset_id).unwrap(),
            Some(dataset.clone())
        );
        assert_eq!(
            list_import_attempts(&connection).unwrap(),
            vec![attempt.clone()]
        );
        assert!(matches!(
            save_dataset_and_attempt(&mut connection, &dataset, &attempt),
            Err(PersistenceError::DatasetAlreadyExists(_))
        ));

        assert!(connection
            .execute(
                "UPDATE datasets SET status = 'BLOCKED' WHERE dataset_id = ?1",
                [&dataset.dataset_id],
            )
            .is_err());
        assert!(connection
            .execute(
                "DELETE FROM datasets WHERE dataset_id = ?1",
                [&dataset.dataset_id],
            )
            .is_err());
        assert!(connection
            .execute(
                "UPDATE import_attempts SET status = 'FAILED' WHERE attempt_id = ?1",
                [&attempt.attempt_id],
            )
            .is_err());
        assert!(connection
            .execute(
                "DELETE FROM import_attempts WHERE attempt_id = ?1",
                [&attempt.attempt_id],
            )
            .is_err());
    }

    #[test]
    fn pending_promotion_journal_completes_dataset_and_attempt_atomically() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let dataset = dataset_metadata();
        let attempt = imported_attempt();
        let pending = PendingDatasetPromotion {
            staging_name: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61".into(),
            dataset: dataset.clone(),
            attempt: attempt.clone(),
        };
        save_pending_dataset_promotion(&connection, &pending).unwrap();
        assert_eq!(
            list_pending_dataset_promotions(&connection).unwrap(),
            vec![pending]
        );
        assert!(get_dataset_metadata(&connection, &dataset.dataset_id)
            .unwrap()
            .is_none());

        complete_pending_dataset_promotion(&mut connection, &dataset.dataset_id).unwrap();
        assert!(list_pending_dataset_promotions(&connection)
            .unwrap()
            .is_empty());
        assert_eq!(
            get_dataset_metadata(&connection, &dataset.dataset_id).unwrap(),
            Some(dataset)
        );
        assert_eq!(list_import_attempts(&connection).unwrap(), vec![attempt]);
    }

    #[test]
    fn failed_pending_promotion_finalization_preserves_journal_for_retry() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let dataset = dataset_metadata();
        save_dataset_and_attempt(&mut connection, &dataset, &imported_attempt()).unwrap();
        let mut pending_attempt = imported_attempt();
        pending_attempt.attempt_id = "attempt_pending_fixture".into();
        let pending = PendingDatasetPromotion {
            staging_name: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c61".into(),
            dataset: dataset.clone(),
            attempt: pending_attempt,
        };
        save_pending_dataset_promotion(&connection, &pending).unwrap();

        assert!(complete_pending_dataset_promotion(&mut connection, &dataset.dataset_id).is_err());
        assert_eq!(
            list_pending_dataset_promotions(&connection).unwrap(),
            vec![pending]
        );
        assert_eq!(list_import_attempts(&connection).unwrap().len(), 1);
    }

    #[test]
    fn dataset_manifest_tampering_is_reported_on_catalog_read() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        let dataset = dataset_metadata();
        save_dataset_and_attempt(&mut connection, &dataset, &imported_attempt()).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER datasets_prevent_update;
                 UPDATE datasets SET manifest_json = manifest_json || ' ';",
            )
            .unwrap();
        let stored = get_dataset_metadata(&connection, &dataset.dataset_id)
            .unwrap()
            .unwrap();
        assert!(!stored.manifest_integrity_valid);
    }

    #[test]
    fn failed_dataset_migration_rolls_back_registry_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        let broken = [
            MIGRATIONS[0],
            MIGRATIONS[1],
            Migration {
                version: 3,
                name: "intentional_dataset_failure_fixture",
                sql:
                    "CREATE TABLE must_rollback(id INTEGER); INSERT INTO missing_table VALUES (1);",
            },
        ];
        assert!(migrate_with(&mut connection, &broken, 3).is_err());
        assert_eq!(schema_version(&connection).unwrap(), 2);
        let datasets: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='datasets'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(datasets, 0);
        let applied: u32 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(applied, 2);
    }
}
