use std::path::Path;

use rg_contracts::ExperimentManifest;
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 7;

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
    #[error("feature set ID or identity already exists: {0}")]
    FeatureSetAlreadyExists(String),
    #[error("feature set metadata is invalid: {0}")]
    InvalidFeatureSetMetadata(String),
    #[error("decision set ID or identity already exists: {0}")]
    DecisionSetAlreadyExists(String),
    #[error("decision set metadata is invalid: {0}")]
    InvalidDecisionSetMetadata(String),
    #[error("schema upgrade was not started because the pre-upgrade backup failed: {0}")]
    SchemaBackupFailed(String),
    #[error("schema backup restore was refused or failed: {0}")]
    SchemaRestoreFailed(String),
    #[error("schema backup does not match its receipt: {0}")]
    SchemaBackupUnverified(String),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
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
    Migration {
        version: 6,
        name: "immutable_feature_set_registry",
        sql: "
        CREATE TABLE feature_sets (
            feature_set_id TEXT PRIMARY KEY NOT NULL,
            identity_sha256 TEXT NOT NULL UNIQUE CHECK (length(identity_sha256) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            output_sha256 TEXT NOT NULL CHECK (length(output_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('READY', 'BLOCKED'))
        );
        CREATE TRIGGER feature_sets_prevent_update
        BEFORE UPDATE ON feature_sets
        BEGIN
            SELECT RAISE(ABORT, 'feature set metadata is immutable');
        END;
        CREATE TRIGGER feature_sets_prevent_delete
        BEFORE DELETE ON feature_sets
        BEGIN
            SELECT RAISE(ABORT, 'feature set metadata is immutable');
        END;
        CREATE TABLE feature_set_inputs (
            feature_set_id TEXT NOT NULL REFERENCES feature_sets(feature_set_id),
            dataset_id TEXT NOT NULL REFERENCES datasets(dataset_id),
            dataset_fingerprint TEXT NOT NULL CHECK (length(dataset_fingerprint) = 64),
            PRIMARY KEY (feature_set_id, dataset_id)
        );
        CREATE TRIGGER feature_set_inputs_prevent_update
        BEFORE UPDATE ON feature_set_inputs
        BEGIN
            SELECT RAISE(ABORT, 'feature set lineage is immutable');
        END;
        CREATE TRIGGER feature_set_inputs_prevent_delete
        BEFORE DELETE ON feature_set_inputs
        BEGIN
            SELECT RAISE(ABORT, 'feature set lineage is immutable');
        END;
        CREATE TABLE feature_set_promotions (
            feature_set_id TEXT PRIMARY KEY NOT NULL,
            staging_name TEXT NOT NULL,
            identity_sha256 TEXT NOT NULL CHECK (length(identity_sha256) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            output_sha256 TEXT NOT NULL CHECK (length(output_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('READY', 'BLOCKED')),
            inputs_json TEXT NOT NULL,
            attempt_id TEXT NOT NULL UNIQUE,
            attempt_started_at_utc TEXT NOT NULL,
            attempt_completed_at_utc TEXT NOT NULL
        );
        CREATE TABLE feature_attempts (
            attempt_id TEXT PRIMARY KEY NOT NULL,
            started_at_utc TEXT NOT NULL,
            completed_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('MATERIALIZED', 'REUSED', 'FAILED', 'CANCELLED')),
            feature_set_id TEXT,
            diagnostic_code TEXT,
            diagnostic_message TEXT
        );
        CREATE INDEX feature_attempts_completed_idx
            ON feature_attempts(completed_at_utc DESC, attempt_id);
        CREATE TRIGGER feature_attempts_prevent_update
        BEFORE UPDATE ON feature_attempts
        BEGIN
            SELECT RAISE(ABORT, 'feature attempt records are immutable');
        END;
        CREATE TRIGGER feature_attempts_prevent_delete
        BEFORE DELETE ON feature_attempts
        BEGIN
            SELECT RAISE(ABORT, 'feature attempt records are immutable');
        END;
    ",
    },
    Migration {
        version: 7,
        name: "immutable_decision_set_registry",
        sql: "
        CREATE TABLE decision_sets (
            decision_set_id TEXT PRIMARY KEY NOT NULL,
            identity_sha256 TEXT NOT NULL UNIQUE CHECK (length(identity_sha256) = 64),
            feature_set_id TEXT NOT NULL REFERENCES feature_sets(feature_set_id),
            feature_set_manifest_sha256 TEXT NOT NULL CHECK (length(feature_set_manifest_sha256) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            output_sha256 TEXT NOT NULL CHECK (length(output_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status = 'READY')
        );
        CREATE TRIGGER decision_sets_prevent_update
        BEFORE UPDATE ON decision_sets
        BEGIN
            SELECT RAISE(ABORT, 'decision set metadata is immutable');
        END;
        CREATE TRIGGER decision_sets_prevent_delete
        BEFORE DELETE ON decision_sets
        BEGIN
            SELECT RAISE(ABORT, 'decision set metadata is immutable');
        END;
        CREATE TABLE decision_set_promotions (
            decision_set_id TEXT PRIMARY KEY NOT NULL,
            staging_name TEXT NOT NULL,
            identity_sha256 TEXT NOT NULL CHECK (length(identity_sha256) = 64),
            feature_set_id TEXT NOT NULL,
            feature_set_manifest_sha256 TEXT NOT NULL CHECK (length(feature_set_manifest_sha256) = 64),
            manifest_json TEXT NOT NULL,
            manifest_sha256 TEXT NOT NULL CHECK (length(manifest_sha256) = 64),
            output_sha256 TEXT NOT NULL CHECK (length(output_sha256) = 64),
            created_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status = 'READY'),
            attempt_id TEXT NOT NULL UNIQUE,
            attempt_started_at_utc TEXT NOT NULL,
            attempt_completed_at_utc TEXT NOT NULL
        );
        CREATE TABLE decision_attempts (
            attempt_id TEXT PRIMARY KEY NOT NULL,
            started_at_utc TEXT NOT NULL,
            completed_at_utc TEXT NOT NULL,
            status TEXT NOT NULL CHECK (status IN ('MATERIALIZED', 'REUSED', 'FAILED', 'CANCELLED')),
            decision_set_id TEXT,
            diagnostic_code TEXT,
            diagnostic_message TEXT
        );
        CREATE INDEX decision_attempts_completed_idx
            ON decision_attempts(completed_at_utc DESC, attempt_id);
        CREATE TRIGGER decision_attempts_prevent_update
        BEFORE UPDATE ON decision_attempts
        BEGIN
            SELECT RAISE(ABORT, 'decision attempt records are immutable');
        END;
        CREATE TRIGGER decision_attempts_prevent_delete
        BEFORE DELETE ON decision_attempts
        BEGIN
            SELECT RAISE(ABORT, 'decision attempt records are immutable');
        END;
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

/// Opens the research database and brings it to the current schema. An existing store below the
/// current schema is first copied to a verified backup (see [`backup_before_upgrade`]); if that
/// fails, the store is left at its version and the open fails.
pub fn open_database(path: impl AsRef<Path>) -> Result<Connection, PersistenceError> {
    open_database_with(path.as_ref(), MIGRATIONS, CURRENT_SCHEMA_VERSION)
}

fn open_database_with(
    path: &Path,
    migrations: &[Migration],
    supported_version: u32,
) -> Result<Connection, PersistenceError> {
    let mut connection = Connection::open(path)?;
    configure(&connection)?;
    let current = schema_version(&connection)?;
    if current > 0 && current < supported_version {
        backup_before_upgrade(&connection, path, current)?;
    }
    migrate_with(&mut connection, migrations, supported_version)?;
    Ok(connection)
}

/// Directory, beside the database, that holds verified pre-upgrade backups.
pub const SCHEMA_BACKUP_DIRECTORY: &str = "schema-backups";

/// A verified, consistent copy of a database taken before a schema upgrade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaBackup {
    pub path: std::path::PathBuf,
    pub schema_version: u32,
    /// SHA-256 over every schema object and row, in a canonical order. Equal digests mean equal
    /// logical content, independent of page layout or journal mode.
    pub content_sha256: String,
}

pub const SCHEMA_BACKUP_RECEIPT_VERSION: u16 = 1;

/// Checksum receipt written beside a backup when it is created, from the source database's own
/// digest. Verification compares a backup against its receipt and never recomputes the expected
/// values from the backup itself. It detects changes to the backup alone; it is not a signature
/// against someone able to rewrite both the backup and its receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaBackupReceipt {
    pub receipt_version: u16,
    pub backup_file_name: String,
    pub source_database_file_name: String,
    pub schema_version: u32,
    pub content_sha256: String,
    pub file_sha256: String,
    pub file_bytes: u64,
}

/// Path of the receipt that belongs to a backup file.
pub fn schema_backup_receipt_path(backup_path: &Path) -> std::path::PathBuf {
    let mut name = backup_path.file_name().unwrap_or_default().to_os_string();
    name.push(".receipt.json");
    backup_path.with_file_name(name)
}

fn file_sha256_and_len(path: &Path) -> Result<(String, u64), std::io::Error> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        hasher.update(&buffer[..count]);
    }
    Ok((
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        total,
    ))
}

fn file_name_text(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

/// Writes a receipt next to its backup atomically: a new temporary file, flushed, then renamed.
fn write_receipt(backup_path: &Path, receipt: &SchemaBackupReceipt) -> Result<(), String> {
    use std::io::Write;
    let target = schema_backup_receipt_path(backup_path);
    let temporary = target.with_extension("tmp");
    let json = serde_json::to_vec_pretty(receipt).map_err(|error| error.to_string())?;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(&json)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, &target));
    written.map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!(
            "could not write the backup receipt {}: {error}",
            target.display()
        )
    })
}

/// Verifies a backup against the receipt written when it was created: the receipt must name this
/// backup and database; the backup's raw SHA-256 and size must match; and SQLite's integrity check,
/// schema version, and logical content digest must match the receipt. Returns the verified backup.
pub fn verify_schema_backup(
    database_path: &Path,
    backup_path: &Path,
) -> Result<SchemaBackup, PersistenceError> {
    let unverified = |reason: String| PersistenceError::SchemaBackupUnverified(reason);
    let directory = std::fs::canonicalize(backup_directory(database_path)?)
        .map_err(|error| unverified(format!("no schema backup directory: {error}")))?;
    let backup = std::fs::canonicalize(backup_path)
        .map_err(|error| unverified(format!("backup is not readable: {error}")))?;
    if backup.parent() != Some(directory.as_path()) || !is_regular_file(&backup) {
        return Err(unverified(
            "backup must be a regular file in the database's schema-backups directory".into(),
        ));
    }
    let receipt_path = schema_backup_receipt_path(&backup);
    if !is_regular_file(&receipt_path) {
        return Err(unverified(format!(
            "receipt {} is missing; the backup's original content cannot be confirmed",
            receipt_path.display()
        )));
    }
    let receipt: SchemaBackupReceipt = serde_json::from_slice(&std::fs::read(&receipt_path)?)
        .map_err(|error| unverified(format!("receipt is malformed: {error}")))?;
    let sha = |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit());
    if receipt.receipt_version != SCHEMA_BACKUP_RECEIPT_VERSION
        || receipt.backup_file_name != file_name_text(&backup)
        || receipt.source_database_file_name != file_name_text(database_path)
        || receipt.schema_version == 0
        || receipt.schema_version > CURRENT_SCHEMA_VERSION
        || !sha(&receipt.content_sha256)
        || !sha(&receipt.file_sha256)
    {
        return Err(unverified(
            "receipt does not describe this backup of this database".into(),
        ));
    }
    let (file_sha256, file_bytes) = file_sha256_and_len(&backup)?;
    if file_sha256 != receipt.file_sha256 || file_bytes != receipt.file_bytes {
        return Err(unverified(
            "backup file bytes changed since the backup was taken".into(),
        ));
    }
    let (version, digest) = verified_snapshot(&backup).map_err(unverified)?;
    if version != receipt.schema_version || digest != receipt.content_sha256 {
        return Err(unverified(
            "backup content no longer matches the content that was backed up".into(),
        ));
    }
    Ok(SchemaBackup {
        path: backup,
        schema_version: receipt.schema_version,
        content_sha256: receipt.content_sha256,
    })
}

#[cfg(test)]
thread_local! {
    /// Test-only fault: report the installed restore as failing its final verification.
    static FAIL_INSTALLED_VERIFICATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Result of restoring a schema backup over a database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredDatabase {
    pub schema_version: u32,
    pub content_sha256: String,
    /// The database files that were moved aside, never deleted.
    pub preserved: Vec<std::path::PathBuf>,
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos}-{}", std::process::id())
}

fn backup_directory(database_path: &Path) -> Result<std::path::PathBuf, PersistenceError> {
    let parent = database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.join(SCHEMA_BACKUP_DIRECTORY))
}

/// Logical SHA-256 of a database: every non-internal schema object (type, name, table, SQL) and
/// every row of every table, in name and rowid order, with typed, length-prefixed values.
pub fn database_content_sha256(connection: &Connection) -> Result<String, PersistenceError> {
    use rusqlite::types::ValueRef;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let field = |hasher: &mut Sha256, bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    };
    let objects: Vec<(String, String, String, Option<String>)> = {
        let mut statement = connection.prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for (kind, name, table, sql) in &objects {
        field(&mut hasher, kind.as_bytes());
        field(&mut hasher, name.as_bytes());
        field(&mut hasher, table.as_bytes());
        field(&mut hasher, sql.as_deref().unwrap_or_default().as_bytes());
    }
    for (_, name, _, _) in objects.iter().filter(|object| object.0 == "table") {
        let mut statement = connection.prepare(&format!(
            "SELECT * FROM \"{}\" ORDER BY rowid",
            name.replace('"', "\"\"")
        ))?;
        let columns = statement.column_count();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            hasher.update([0xff]);
            for index in 0..columns {
                match row.get_ref(index)? {
                    ValueRef::Null => hasher.update([0]),
                    ValueRef::Integer(value) => {
                        hasher.update([1]);
                        hasher.update(value.to_be_bytes());
                    }
                    ValueRef::Real(value) => {
                        hasher.update([2]);
                        hasher.update(value.to_bits().to_be_bytes());
                    }
                    ValueRef::Text(value) => {
                        hasher.update([3]);
                        field(&mut hasher, value);
                    }
                    ValueRef::Blob(value) => {
                        hasher.update([4]);
                        field(&mut hasher, value);
                    }
                }
            }
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Opens a backup or candidate file read-only and returns its schema version and content
/// digest after SQLite's full integrity check passes.
fn verified_snapshot(path: &Path) -> Result<(u32, String), String> {
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|error| format!("integrity check failed: {error}"))?;
    if integrity != "ok" {
        return Err(format!("integrity check reported: {integrity}"));
    }
    let version = schema_version(&connection).map_err(|error| error.to_string())?;
    let digest = database_content_sha256(&connection).map_err(|error| error.to_string())?;
    Ok((version, digest))
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

/// Writes a consistent copy of the open database to the backup directory and verifies it before
/// any schema change. `VACUUM INTO` reads through the connection, so committed content that is
/// still only in the WAL file is included. A full disk or any other write failure surfaces as an
/// error here, before the upgrade starts. The copy must pass SQLite's integrity check and match the
/// source's schema version and logical content digest; otherwise it is removed and the upgrade is
/// refused.
pub fn backup_before_upgrade(
    connection: &Connection,
    database_path: &Path,
    schema_version_before: u32,
) -> Result<SchemaBackup, PersistenceError> {
    let failed = |reason: String| PersistenceError::SchemaBackupFailed(reason);
    let directory = backup_directory(database_path)?;
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(failed(format!(
                "{} exists but is not a private directory",
                directory.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&directory).map_err(|error| {
                failed(format!("cannot create {}: {error}", directory.display()))
            })?
        }
        Err(error) => return Err(failed(error.to_string())),
    }
    let stem = database_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("research.sqlite3");
    let target = directory.join(format!(
        "{stem}.schema-v{schema_version_before}.{}.sqlite3",
        unique_suffix()
    ));
    let target_text = target
        .to_str()
        .ok_or_else(|| failed("backup path is not valid UTF-8".into()))?;
    let source_digest = database_content_sha256(connection)?;
    let result = connection
        .execute("VACUUM INTO ?1", [target_text])
        .map_err(|error| failed(format!("could not write {}: {error}", target.display())))
        .and_then(|_| verified_snapshot(&target).map_err(failed))
        .and_then(|(version, digest)| {
            if version != schema_version_before || digest != source_digest {
                Err(failed(
                    "backup does not reproduce the source schema version and content".into(),
                ))
            } else {
                Ok(digest)
            }
        });
    // The receipt anchors what was backed up, using the source's own digest. Without it the
    // backup cannot be trusted later, so its absence is a failed backup.
    let result = result.and_then(|content_sha256| {
        let (file_sha256, file_bytes) =
            file_sha256_and_len(&target).map_err(|error| failed(error.to_string()))?;
        write_receipt(
            &target,
            &SchemaBackupReceipt {
                receipt_version: SCHEMA_BACKUP_RECEIPT_VERSION,
                backup_file_name: file_name_text(&target),
                source_database_file_name: file_name_text(database_path),
                schema_version: schema_version_before,
                content_sha256: content_sha256.clone(),
                file_sha256,
                file_bytes,
            },
        )
        .map_err(failed)?;
        Ok(content_sha256)
    });
    match result {
        Ok(content_sha256) => Ok(SchemaBackup {
            path: target,
            schema_version: schema_version_before,
            content_sha256,
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&target);
            Err(error)
        }
    }
}

/// Lists pre-upgrade backups beside a database, oldest first.
pub fn list_schema_backups(
    database_path: &Path,
) -> Result<Vec<std::path::PathBuf>, PersistenceError> {
    let directory = backup_directory(database_path)?;
    let mut backups = match std::fs::read_dir(&directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    backups.retain(|path| {
        is_regular_file(path)
            && path
                .extension()
                .is_some_and(|extension| extension == "sqlite3")
    });
    backups.sort();
    Ok(backups)
}

/// Restores a verified pre-upgrade backup as the database, for example so a previously accepted
/// application build can open it again. The backup must match the receipt written when it was
/// taken ([`verify_schema_backup`]). The application must be closed: the restore is refused while
/// any connection keeps the database busy. The current database, WAL, and shared-memory files are
/// renamed aside, never deleted, and the installed copy is verified against the receipt. If that
/// final verification fails, the installed copy is moved aside and the original files are put back.
pub fn restore_schema_backup(
    database_path: &Path,
    backup_path: &Path,
) -> Result<RestoredDatabase, PersistenceError> {
    let refused = |reason: String| PersistenceError::SchemaRestoreFailed(reason);
    let verified = verify_schema_backup(database_path, backup_path)
        .map_err(|error| refused(error.to_string()))?;
    let (backup, version, digest) = (
        verified.path,
        verified.schema_version,
        verified.content_sha256,
    );

    // Early refusal when a checkpoint cannot complete. It does not catch every open connection;
    // the file moves below are the authoritative check and undo themselves on failure.
    if is_regular_file(database_path) {
        let connection = Connection::open(database_path)?;
        let busy: i64 = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .map_err(|error| refused(format!("database is in use: {error}")))?;
        if busy != 0 {
            return Err(refused(
                "database is in use; close the application before restoring".into(),
            ));
        }
    }

    let suffix = unique_suffix();
    let candidate = database_path.with_file_name(format!(
        "{}.restore-{suffix}.tmp",
        database_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("research.sqlite3")
    ));
    {
        let source =
            Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let candidate_text = candidate
            .to_str()
            .ok_or_else(|| refused("database path is not valid UTF-8".into()))?;
        source
            .execute("VACUUM INTO ?1", [candidate_text])
            .map_err(|error| refused(format!("could not stage the restore: {error}")))?;
    }
    match verified_snapshot(&candidate) {
        Ok((candidate_version, candidate_digest))
            if candidate_version == version && candidate_digest == digest => {}
        _ => {
            let _ = std::fs::remove_file(&candidate);
            return Err(refused("staged restore does not match the backup".into()));
        }
    }

    let mut preserved = Vec::new();
    let aside = |path: &Path| {
        path.with_file_name(format!(
            "{}.pre-restore-{suffix}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("database")
        ))
    };
    let wal = database_path.with_file_name(format!(
        "{}-wal",
        database_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("research.sqlite3")
    ));
    let shm = database_path.with_file_name(format!(
        "{}-shm",
        database_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("research.sqlite3")
    ));
    for path in [database_path.to_path_buf(), wal, shm] {
        if std::fs::symlink_metadata(&path).is_ok() {
            let destination = aside(&path);
            if let Err(error) = std::fs::rename(&path, &destination) {
                // Put back whatever was already moved, then fail without touching anything else.
                for (original, moved) in preserved.iter().rev() {
                    let _ = std::fs::rename(moved, original);
                }
                let _ = std::fs::remove_file(&candidate);
                // Windows refuses to rename a file another process or connection holds open
                // (sharing or lock violation): the authoritative "in use" signal.
                let reason = if matches!(error.raw_os_error(), Some(32 | 33)) {
                    format!(
                        "database is in use; close the application before restoring ({})",
                        path.display()
                    )
                } else {
                    format!("could not move {} aside: {error}", path.display())
                };
                return Err(refused(reason));
            }
            preserved.push((path, destination));
        }
    }
    if let Err(error) = std::fs::rename(&candidate, database_path) {
        for (original, moved) in preserved.iter().rev() {
            let _ = std::fs::rename(moved, original);
        }
        return Err(refused(format!(
            "could not install the restored database: {error}"
        )));
    }
    let installed = verified_snapshot(database_path);
    #[cfg(test)]
    let installed = if FAIL_INSTALLED_VERIFICATION.with(std::cell::Cell::get) {
        Err("injected installed-copy verification failure".to_owned())
    } else {
        installed
    };
    if !matches!(&installed, Ok((v, d)) if *v == version && *d == digest) {
        // Never report success for an unverified install: move it aside and put the originals
        // back. If any step of that fails, name where each state now is.
        let failed_copy = database_path.with_file_name(format!(
            "{}.failed-restore-{suffix}",
            file_name_text(database_path)
        ));
        let mut problems = Vec::new();
        if let Err(error) = std::fs::rename(database_path, &failed_copy) {
            problems.push(format!("could not move the failed copy aside: {error}"));
        } else {
            for (original, moved) in preserved.iter().rev() {
                if let Err(error) = std::fs::rename(moved, original) {
                    problems.push(format!(
                        "original {} is still at {}: {error}",
                        original.display(),
                        moved.display()
                    ));
                }
            }
        }
        let reason = match &installed {
            Ok(_) => "restored copy does not match the backup receipt".to_owned(),
            Err(error) => format!("restored copy failed verification: {error}"),
        };
        return Err(refused(if problems.is_empty() {
            format!(
                "{reason}; the original database was put back and the failed copy is kept at {}",
                failed_copy.display()
            )
        } else {
            format!(
                "{reason}; rollback is incomplete and needs manual recovery: {}",
                problems.join("; ")
            )
        }));
    }
    Ok(RestoredDatabase {
        schema_version: version,
        content_sha256: digest,
        preserved: preserved.into_iter().map(|(_, moved)| moved).collect(),
    })
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

/// Brings a connection to an older schema version, as a previously accepted build would leave it.
/// For upgrade and restore tests only.
#[doc(hidden)]
pub fn migrate_to_version_for_tests(
    connection: &mut Connection,
    version: u32,
) -> Result<u32, PersistenceError> {
    let count = usize::try_from(version)
        .unwrap_or(usize::MAX)
        .min(MIGRATIONS.len());
    migrate_with(connection, &MIGRATIONS[..count], version)
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureSetMetadata {
    pub feature_set_id: String,
    pub identity_sha256: String,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub output_sha256: String,
    pub created_at_utc: String,
    pub status: String,
    pub manifest_integrity_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSetInput {
    pub dataset_id: String,
    pub dataset_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FeatureAttemptRecord {
    pub attempt_id: String,
    pub started_at_utc: String,
    pub completed_at_utc: String,
    pub status: String,
    pub feature_set_id: Option<String>,
    pub diagnostic_code: Option<String>,
    pub diagnostic_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingFeatureSetPromotion {
    pub staging_name: String,
    pub feature_set: FeatureSetMetadata,
    pub inputs: Vec<FeatureSetInput>,
    pub attempt: FeatureAttemptRecord,
}

fn validate_feature_set_metadata(feature_set: &FeatureSetMetadata) -> Result<(), PersistenceError> {
    let valid_sha =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid_sha(&feature_set.identity_sha256)
        || feature_set.feature_set_id != format!("fs_{}", feature_set.identity_sha256)
        || !valid_sha(&feature_set.manifest_sha256)
        || !valid_sha(&feature_set.output_sha256)
        || feature_set.created_at_utc.trim().is_empty()
        || !matches!(feature_set.status.as_str(), "READY" | "BLOCKED")
    {
        return Err(PersistenceError::InvalidFeatureSetMetadata(
            "identifier, digest, timestamp, or status is invalid".into(),
        ));
    }
    let actual = rg_contracts::Sha256Digest::of_bytes(feature_set.manifest_json.as_bytes());
    if feature_set.manifest_sha256 != actual.as_str() {
        return Err(PersistenceError::InvalidFeatureSetMetadata(
            "manifest SHA-256 does not match manifest bytes".into(),
        ));
    }
    Ok(())
}

fn validate_feature_inputs(inputs: &[FeatureSetInput]) -> Result<(), PersistenceError> {
    let valid_sha =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    let mut seen = std::collections::BTreeSet::new();
    if inputs.is_empty()
        || inputs.iter().any(|input| {
            !valid_sha(&input.dataset_fingerprint)
                || input.dataset_id != format!("ds_{}", input.dataset_fingerprint)
                || !seen.insert(input.dataset_id.as_str())
        })
    {
        return Err(PersistenceError::InvalidFeatureSetMetadata(
            "feature set lineage must name distinct registered dataset identities".into(),
        ));
    }
    Ok(())
}

fn validate_feature_attempt(
    attempt: &FeatureAttemptRecord,
    allowed: &[&str],
) -> Result<(), PersistenceError> {
    if attempt.attempt_id.trim().is_empty()
        || attempt.started_at_utc.trim().is_empty()
        || attempt.completed_at_utc.trim().is_empty()
        || !allowed.contains(&attempt.status.as_str())
    {
        return Err(PersistenceError::InvalidFeatureSetMetadata(
            "feature attempt metadata is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_staging_name(value: &str) -> Result<(), PersistenceError> {
    if value.trim().is_empty() || value.contains(['/', '\\']) || value == "." || value == ".." {
        return Err(PersistenceError::InvalidFeatureSetMetadata(
            "promotion staging identity is invalid".into(),
        ));
    }
    Ok(())
}

pub fn save_pending_feature_set_promotion(
    connection: &Connection,
    pending: &PendingFeatureSetPromotion,
) -> Result<(), PersistenceError> {
    validate_feature_set_metadata(&pending.feature_set)?;
    validate_feature_inputs(&pending.inputs)?;
    validate_feature_attempt(&pending.attempt, &["MATERIALIZED"])?;
    validate_staging_name(&pending.staging_name)?;
    let inputs_json = serde_json::to_string(&pending.inputs)?;
    connection.execute(
        "INSERT INTO feature_set_promotions (
            feature_set_id, staging_name, identity_sha256, manifest_json, manifest_sha256,
            output_sha256, created_at_utc, status, inputs_json, attempt_id,
            attempt_started_at_utc, attempt_completed_at_utc
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            pending.feature_set.feature_set_id,
            pending.staging_name,
            pending.feature_set.identity_sha256,
            pending.feature_set.manifest_json,
            pending.feature_set.manifest_sha256,
            pending.feature_set.output_sha256,
            pending.feature_set.created_at_utc,
            pending.feature_set.status,
            inputs_json,
            pending.attempt.attempt_id,
            pending.attempt.started_at_utc,
            pending.attempt.completed_at_utc,
        ],
    )?;
    Ok(())
}

fn pending_feature_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<(PendingFeatureSetPromotion, String), rusqlite::Error> {
    let manifest_json: String = row.get(3)?;
    let manifest_sha256: String = row.get(4)?;
    let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
    let feature_set_id: String = row.get(0)?;
    Ok((
        PendingFeatureSetPromotion {
            staging_name: row.get(1)?,
            feature_set: FeatureSetMetadata {
                feature_set_id: feature_set_id.clone(),
                identity_sha256: row.get(2)?,
                manifest_integrity_valid: manifest_sha256 == actual.as_str(),
                manifest_json,
                manifest_sha256,
                output_sha256: row.get(5)?,
                created_at_utc: row.get(6)?,
                status: row.get(7)?,
            },
            inputs: Vec::new(),
            attempt: FeatureAttemptRecord {
                attempt_id: row.get(9)?,
                started_at_utc: row.get(10)?,
                completed_at_utc: row.get(11)?,
                status: "MATERIALIZED".into(),
                feature_set_id: Some(feature_set_id),
                diagnostic_code: None,
                diagnostic_message: None,
            },
        },
        row.get(8)?,
    ))
}

const PENDING_FEATURE_COLUMNS: &str =
    "feature_set_id, staging_name, identity_sha256, manifest_json, manifest_sha256,
     output_sha256, created_at_utc, status, inputs_json, attempt_id,
     attempt_started_at_utc, attempt_completed_at_utc";

fn decode_pending_feature(
    (mut pending, inputs_json): (PendingFeatureSetPromotion, String),
) -> Result<PendingFeatureSetPromotion, PersistenceError> {
    pending.inputs = serde_json::from_str(&inputs_json)?;
    validate_feature_set_metadata(&pending.feature_set)?;
    validate_feature_inputs(&pending.inputs)?;
    validate_staging_name(&pending.staging_name)?;
    Ok(pending)
}

pub fn list_pending_feature_set_promotions(
    connection: &Connection,
) -> Result<Vec<PendingFeatureSetPromotion>, PersistenceError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {PENDING_FEATURE_COLUMNS} FROM feature_set_promotions ORDER BY feature_set_id"
    ))?;
    let rows = statement.query_map([], pending_feature_from_row)?;
    rows.collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(decode_pending_feature)
        .collect()
}

pub fn clear_pending_feature_set_promotion(
    connection: &Connection,
    feature_set_id: &str,
) -> Result<bool, PersistenceError> {
    Ok(connection.execute(
        "DELETE FROM feature_set_promotions WHERE feature_set_id = ?1",
        [feature_set_id],
    )? == 1)
}

/// Registers the journaled feature set, its dataset lineage, and its terminal attempt in one
/// transaction, then removes the journal entry. A failure leaves the journal for recovery.
pub fn complete_pending_feature_set_promotion(
    connection: &mut Connection,
    feature_set_id: &str,
) -> Result<(), PersistenceError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending = {
        let mut statement = tx.prepare(&format!(
            "SELECT {PENDING_FEATURE_COLUMNS} FROM feature_set_promotions WHERE feature_set_id = ?1"
        ))?;
        let mut rows = statement.query([feature_set_id])?;
        rows.next()?.map(pending_feature_from_row).transpose()?
    }
    .ok_or_else(|| {
        PersistenceError::InvalidFeatureSetMetadata(format!(
            "pending promotion is missing for feature set {feature_set_id}"
        ))
    })
    .and_then(decode_pending_feature)?;
    let feature_set = &pending.feature_set;
    let inserted = tx.execute(
        "INSERT INTO feature_sets (
            feature_set_id, identity_sha256, manifest_json, manifest_sha256, output_sha256,
            created_at_utc, status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            feature_set.feature_set_id,
            feature_set.identity_sha256,
            feature_set.manifest_json,
            feature_set.manifest_sha256,
            feature_set.output_sha256,
            feature_set.created_at_utc,
            feature_set.status,
        ],
    );
    match inserted {
        Ok(_) => {}
        Err(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
        {
            return Err(PersistenceError::FeatureSetAlreadyExists(
                feature_set.feature_set_id.clone(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    for input in &pending.inputs {
        tx.execute(
            "INSERT INTO feature_set_inputs (feature_set_id, dataset_id, dataset_fingerprint)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                feature_set.feature_set_id,
                input.dataset_id,
                input.dataset_fingerprint
            ],
        )?;
    }
    insert_feature_attempt(&tx, &pending.attempt)?;
    tx.execute(
        "DELETE FROM feature_set_promotions WHERE feature_set_id = ?1",
        [feature_set_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn insert_feature_attempt(
    connection: &Connection,
    attempt: &FeatureAttemptRecord,
) -> Result<(), PersistenceError> {
    connection.execute(
        "INSERT INTO feature_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, feature_set_id,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            attempt.attempt_id,
            attempt.started_at_utc,
            attempt.completed_at_utc,
            attempt.status,
            attempt.feature_set_id,
            attempt.diagnostic_code,
            attempt.diagnostic_message
        ],
    )?;
    Ok(())
}

pub fn save_feature_attempt(
    connection: &Connection,
    attempt: &FeatureAttemptRecord,
) -> Result<(), PersistenceError> {
    validate_feature_attempt(attempt, &["REUSED", "FAILED", "CANCELLED"])?;
    insert_feature_attempt(connection, attempt)
}

pub fn list_feature_attempts(
    connection: &Connection,
    limit: u64,
) -> Result<Vec<FeatureAttemptRecord>, PersistenceError> {
    let limit = i64::try_from(limit).map_err(|_| {
        PersistenceError::InvalidFeatureSetMetadata("attempt page size is too large".into())
    })?;
    let mut statement = connection.prepare(
        "SELECT attempt_id, started_at_utc, completed_at_utc, status, feature_set_id,
                diagnostic_code, diagnostic_message
         FROM feature_attempts ORDER BY completed_at_utc DESC, attempt_id LIMIT ?1",
    )?;
    let rows = statement.query_map([limit], |row| {
        Ok(FeatureAttemptRecord {
            attempt_id: row.get(0)?,
            started_at_utc: row.get(1)?,
            completed_at_utc: row.get(2)?,
            status: row.get(3)?,
            feature_set_id: row.get(4)?,
            diagnostic_code: row.get(5)?,
            diagnostic_message: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn feature_set_from_row(row: &rusqlite::Row<'_>) -> Result<FeatureSetMetadata, rusqlite::Error> {
    let manifest_json: String = row.get(2)?;
    let manifest_sha256: String = row.get(3)?;
    let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
    Ok(FeatureSetMetadata {
        feature_set_id: row.get(0)?,
        identity_sha256: row.get(1)?,
        manifest_integrity_valid: manifest_sha256 == actual.as_str(),
        manifest_json,
        manifest_sha256,
        output_sha256: row.get(4)?,
        created_at_utc: row.get(5)?,
        status: row.get(6)?,
    })
}

const FEATURE_SET_COLUMNS: &str = "feature_set_id, identity_sha256, manifest_json, manifest_sha256,
     output_sha256, created_at_utc, status";

pub fn get_feature_set_metadata(
    connection: &Connection,
    feature_set_id: &str,
) -> Result<Option<FeatureSetMetadata>, PersistenceError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {FEATURE_SET_COLUMNS} FROM feature_sets WHERE feature_set_id = ?1"
    ))?;
    let mut rows = statement.query([feature_set_id])?;
    Ok(rows.next()?.map(feature_set_from_row).transpose()?)
}

pub fn list_feature_set_metadata_after(
    connection: &Connection,
    after_feature_set_id: &str,
    limit: u64,
) -> Result<Vec<FeatureSetMetadata>, PersistenceError> {
    let limit = i64::try_from(limit).map_err(|_| {
        PersistenceError::InvalidFeatureSetMetadata("feature set page size is too large".into())
    })?;
    let mut statement = connection.prepare(&format!(
        "SELECT {FEATURE_SET_COLUMNS} FROM feature_sets
         WHERE feature_set_id > ?1 ORDER BY feature_set_id LIMIT ?2"
    ))?;
    let rows = statement.query_map(
        rusqlite::params![after_feature_set_id, limit],
        feature_set_from_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

pub fn list_feature_set_inputs(
    connection: &Connection,
    feature_set_id: &str,
) -> Result<Vec<FeatureSetInput>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT dataset_id, dataset_fingerprint FROM feature_set_inputs
         WHERE feature_set_id = ?1 ORDER BY dataset_id",
    )?;
    let rows = statement.query_map([feature_set_id], |row| {
        Ok(FeatureSetInput {
            dataset_id: row.get(0)?,
            dataset_fingerprint: row.get(1)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionSetMetadata {
    pub decision_set_id: String,
    pub identity_sha256: String,
    pub feature_set_id: String,
    pub feature_set_manifest_sha256: String,
    pub manifest_json: String,
    pub manifest_sha256: String,
    pub output_sha256: String,
    pub created_at_utc: String,
    pub status: String,
    pub manifest_integrity_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DecisionAttemptRecord {
    pub attempt_id: String,
    pub started_at_utc: String,
    pub completed_at_utc: String,
    pub status: String,
    pub decision_set_id: Option<String>,
    pub diagnostic_code: Option<String>,
    pub diagnostic_message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDecisionSetPromotion {
    pub staging_name: String,
    pub decision_set: DecisionSetMetadata,
    pub attempt: DecisionAttemptRecord,
}

fn validate_decision_set_metadata(
    decision_set: &DecisionSetMetadata,
) -> Result<(), PersistenceError> {
    let valid_sha =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    let valid_feature_set = decision_set
        .feature_set_id
        .strip_prefix("fs_")
        .is_some_and(valid_sha);
    if !valid_sha(&decision_set.identity_sha256)
        || decision_set.decision_set_id != format!("dc_{}", decision_set.identity_sha256)
        || !valid_feature_set
        || !valid_sha(&decision_set.feature_set_manifest_sha256)
        || !valid_sha(&decision_set.manifest_sha256)
        || !valid_sha(&decision_set.output_sha256)
        || decision_set.created_at_utc.trim().is_empty()
        || decision_set.status != "READY"
    {
        return Err(PersistenceError::InvalidDecisionSetMetadata(
            "identifier, feature-set lineage, digest, timestamp, or status is invalid".into(),
        ));
    }
    let actual = rg_contracts::Sha256Digest::of_bytes(decision_set.manifest_json.as_bytes());
    if decision_set.manifest_sha256 != actual.as_str() {
        return Err(PersistenceError::InvalidDecisionSetMetadata(
            "manifest SHA-256 does not match manifest bytes".into(),
        ));
    }
    Ok(())
}

fn validate_decision_attempt(
    attempt: &DecisionAttemptRecord,
    allowed: &[&str],
) -> Result<(), PersistenceError> {
    if attempt.attempt_id.trim().is_empty()
        || attempt.started_at_utc.trim().is_empty()
        || attempt.completed_at_utc.trim().is_empty()
        || !allowed.contains(&attempt.status.as_str())
    {
        return Err(PersistenceError::InvalidDecisionSetMetadata(
            "decision attempt metadata is invalid".into(),
        ));
    }
    Ok(())
}

pub fn save_pending_decision_set_promotion(
    connection: &Connection,
    pending: &PendingDecisionSetPromotion,
) -> Result<(), PersistenceError> {
    validate_decision_set_metadata(&pending.decision_set)?;
    validate_decision_attempt(&pending.attempt, &["MATERIALIZED"])?;
    validate_staging_name(&pending.staging_name)?;
    let decision_set = &pending.decision_set;
    connection.execute(
        "INSERT INTO decision_set_promotions (
            decision_set_id, staging_name, identity_sha256, feature_set_id,
            feature_set_manifest_sha256, manifest_json, manifest_sha256, output_sha256,
            created_at_utc, status, attempt_id, attempt_started_at_utc, attempt_completed_at_utc
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            decision_set.decision_set_id,
            pending.staging_name,
            decision_set.identity_sha256,
            decision_set.feature_set_id,
            decision_set.feature_set_manifest_sha256,
            decision_set.manifest_json,
            decision_set.manifest_sha256,
            decision_set.output_sha256,
            decision_set.created_at_utc,
            decision_set.status,
            pending.attempt.attempt_id,
            pending.attempt.started_at_utc,
            pending.attempt.completed_at_utc,
        ],
    )?;
    Ok(())
}

const PENDING_DECISION_COLUMNS: &str =
    "decision_set_id, staging_name, identity_sha256, feature_set_id, feature_set_manifest_sha256,
     manifest_json, manifest_sha256, output_sha256, created_at_utc, status, attempt_id,
     attempt_started_at_utc, attempt_completed_at_utc";

fn pending_decision_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<PendingDecisionSetPromotion, rusqlite::Error> {
    let manifest_json: String = row.get(5)?;
    let manifest_sha256: String = row.get(6)?;
    let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
    let decision_set_id: String = row.get(0)?;
    Ok(PendingDecisionSetPromotion {
        staging_name: row.get(1)?,
        decision_set: DecisionSetMetadata {
            decision_set_id: decision_set_id.clone(),
            identity_sha256: row.get(2)?,
            feature_set_id: row.get(3)?,
            feature_set_manifest_sha256: row.get(4)?,
            manifest_integrity_valid: manifest_sha256 == actual.as_str(),
            manifest_json,
            manifest_sha256,
            output_sha256: row.get(7)?,
            created_at_utc: row.get(8)?,
            status: row.get(9)?,
        },
        attempt: DecisionAttemptRecord {
            attempt_id: row.get(10)?,
            started_at_utc: row.get(11)?,
            completed_at_utc: row.get(12)?,
            status: "MATERIALIZED".into(),
            decision_set_id: Some(decision_set_id),
            diagnostic_code: None,
            diagnostic_message: None,
        },
    })
}

fn decode_pending_decision(
    pending: PendingDecisionSetPromotion,
) -> Result<PendingDecisionSetPromotion, PersistenceError> {
    validate_decision_set_metadata(&pending.decision_set)?;
    validate_staging_name(&pending.staging_name)?;
    Ok(pending)
}

pub fn list_pending_decision_set_promotions(
    connection: &Connection,
) -> Result<Vec<PendingDecisionSetPromotion>, PersistenceError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {PENDING_DECISION_COLUMNS} FROM decision_set_promotions ORDER BY decision_set_id"
    ))?;
    let rows = statement.query_map([], pending_decision_from_row)?;
    rows.collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(decode_pending_decision)
        .collect()
}

pub fn clear_pending_decision_set_promotion(
    connection: &Connection,
    decision_set_id: &str,
) -> Result<bool, PersistenceError> {
    Ok(connection.execute(
        "DELETE FROM decision_set_promotions WHERE decision_set_id = ?1",
        [decision_set_id],
    )? == 1)
}

/// Registers the journaled decision set and its terminal attempt in one transaction, then removes
/// the journal entry. A failure, such as lineage to an unregistered feature set, leaves the
/// journal for recovery.
pub fn complete_pending_decision_set_promotion(
    connection: &mut Connection,
    decision_set_id: &str,
) -> Result<(), PersistenceError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let pending = {
        let mut statement = tx.prepare(&format!(
            "SELECT {PENDING_DECISION_COLUMNS} FROM decision_set_promotions WHERE decision_set_id = ?1"
        ))?;
        let mut rows = statement.query([decision_set_id])?;
        rows.next()?.map(pending_decision_from_row).transpose()?
    }
    .ok_or_else(|| {
        PersistenceError::InvalidDecisionSetMetadata(format!(
            "pending promotion is missing for decision set {decision_set_id}"
        ))
    })
    .and_then(decode_pending_decision)?;
    let decision_set = &pending.decision_set;
    let inserted = tx.execute(
        "INSERT INTO decision_sets (
            decision_set_id, identity_sha256, feature_set_id, feature_set_manifest_sha256,
            manifest_json, manifest_sha256, output_sha256, created_at_utc, status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            decision_set.decision_set_id,
            decision_set.identity_sha256,
            decision_set.feature_set_id,
            decision_set.feature_set_manifest_sha256,
            decision_set.manifest_json,
            decision_set.manifest_sha256,
            decision_set.output_sha256,
            decision_set.created_at_utc,
            decision_set.status,
        ],
    );
    match inserted {
        Ok(_) => {}
        Err(error)
            if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation)
                && get_feature_set_metadata(&tx, &decision_set.feature_set_id)?.is_some() =>
        {
            return Err(PersistenceError::DecisionSetAlreadyExists(
                decision_set.decision_set_id.clone(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    insert_decision_attempt(&tx, &pending.attempt)?;
    tx.execute(
        "DELETE FROM decision_set_promotions WHERE decision_set_id = ?1",
        [decision_set_id],
    )?;
    tx.commit()?;
    Ok(())
}

fn insert_decision_attempt(
    connection: &Connection,
    attempt: &DecisionAttemptRecord,
) -> Result<(), PersistenceError> {
    connection.execute(
        "INSERT INTO decision_attempts (
            attempt_id, started_at_utc, completed_at_utc, status, decision_set_id,
            diagnostic_code, diagnostic_message
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            attempt.attempt_id,
            attempt.started_at_utc,
            attempt.completed_at_utc,
            attempt.status,
            attempt.decision_set_id,
            attempt.diagnostic_code,
            attempt.diagnostic_message
        ],
    )?;
    Ok(())
}

pub fn save_decision_attempt(
    connection: &Connection,
    attempt: &DecisionAttemptRecord,
) -> Result<(), PersistenceError> {
    validate_decision_attempt(attempt, &["REUSED", "FAILED", "CANCELLED"])?;
    insert_decision_attempt(connection, attempt)
}

pub fn list_decision_attempts(
    connection: &Connection,
    limit: u64,
) -> Result<Vec<DecisionAttemptRecord>, PersistenceError> {
    let limit = i64::try_from(limit).map_err(|_| {
        PersistenceError::InvalidDecisionSetMetadata("attempt page size is too large".into())
    })?;
    let mut statement = connection.prepare(
        "SELECT attempt_id, started_at_utc, completed_at_utc, status, decision_set_id,
                diagnostic_code, diagnostic_message
         FROM decision_attempts ORDER BY completed_at_utc DESC, attempt_id LIMIT ?1",
    )?;
    let rows = statement.query_map([limit], |row| {
        Ok(DecisionAttemptRecord {
            attempt_id: row.get(0)?,
            started_at_utc: row.get(1)?,
            completed_at_utc: row.get(2)?,
            status: row.get(3)?,
            decision_set_id: row.get(4)?,
            diagnostic_code: row.get(5)?,
            diagnostic_message: row.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn decision_set_from_row(row: &rusqlite::Row<'_>) -> Result<DecisionSetMetadata, rusqlite::Error> {
    let manifest_json: String = row.get(4)?;
    let manifest_sha256: String = row.get(5)?;
    let actual = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes());
    Ok(DecisionSetMetadata {
        decision_set_id: row.get(0)?,
        identity_sha256: row.get(1)?,
        feature_set_id: row.get(2)?,
        feature_set_manifest_sha256: row.get(3)?,
        manifest_integrity_valid: manifest_sha256 == actual.as_str(),
        manifest_json,
        manifest_sha256,
        output_sha256: row.get(6)?,
        created_at_utc: row.get(7)?,
        status: row.get(8)?,
    })
}

const DECISION_SET_COLUMNS: &str = "decision_set_id, identity_sha256, feature_set_id,
     feature_set_manifest_sha256, manifest_json, manifest_sha256, output_sha256, created_at_utc,
     status";

pub fn get_decision_set_metadata(
    connection: &Connection,
    decision_set_id: &str,
) -> Result<Option<DecisionSetMetadata>, PersistenceError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {DECISION_SET_COLUMNS} FROM decision_sets WHERE decision_set_id = ?1"
    ))?;
    let mut rows = statement.query([decision_set_id])?;
    Ok(rows.next()?.map(decision_set_from_row).transpose()?)
}

pub fn list_decision_set_metadata_after(
    connection: &Connection,
    after_decision_set_id: &str,
    limit: u64,
) -> Result<Vec<DecisionSetMetadata>, PersistenceError> {
    let limit = i64::try_from(limit).map_err(|_| {
        PersistenceError::InvalidDecisionSetMetadata("decision set page size is too large".into())
    })?;
    let mut statement = connection.prepare(&format!(
        "SELECT {DECISION_SET_COLUMNS} FROM decision_sets
         WHERE decision_set_id > ?1 ORDER BY decision_set_id LIMIT ?2"
    ))?;
    let rows = statement.query_map(
        rusqlite::params![after_decision_set_id, limit],
        decision_set_from_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
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
        assert_eq!(count, CURRENT_SCHEMA_VERSION);
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
            assert_eq!(migrations, CURRENT_SCHEMA_VERSION);
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
        assert_failed_dataset_migration_left_no_registry(&connection);
    }

    fn assert_failed_dataset_migration_left_no_registry(connection: &Connection) {
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

    /// Registers a READY fixture dataset whose ID follows the `ds_<fingerprint>` contract so
    /// feature lineage foreign keys have a real parent row.
    fn registered_lineage_dataset(connection: &mut Connection, seed: char) -> FeatureSetInput {
        let fingerprint = seed.to_string().repeat(64);
        let manifest_json = format!("{{\"fixture\":\"lineage-{seed}\"}}");
        let dataset = DatasetMetadata {
            dataset_id: format!("ds_{fingerprint}"),
            fingerprint: fingerprint.clone(),
            manifest_sha256: checksum(&manifest_json),
            manifest_json,
            parquet_sha256: "b".repeat(64),
            raw_sha256: "c".repeat(64),
            created_at_utc: "2026-10-20T00:00:00Z".into(),
            status: "READY".into(),
            manifest_integrity_valid: true,
        };
        let mut attempt = imported_attempt();
        attempt.attempt_id = format!("attempt-lineage-{seed}");
        save_dataset_and_attempt(connection, &dataset, &attempt).unwrap();
        FeatureSetInput {
            dataset_id: dataset.dataset_id,
            dataset_fingerprint: fingerprint,
        }
    }

    fn pending_feature_set(inputs: Vec<FeatureSetInput>) -> PendingFeatureSetPromotion {
        let identity = "f".repeat(64);
        let manifest_json = "{\"feature_fixture\":true}".to_owned();
        PendingFeatureSetPromotion {
            staging_name: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c62".into(),
            feature_set: FeatureSetMetadata {
                feature_set_id: format!("fs_{identity}"),
                identity_sha256: identity,
                manifest_sha256: checksum(&manifest_json),
                manifest_json,
                output_sha256: "e".repeat(64),
                created_at_utc: "2026-10-20T00:00:01Z".into(),
                status: "READY".into(),
                manifest_integrity_valid: true,
            },
            inputs,
            attempt: FeatureAttemptRecord {
                attempt_id: "feature-attempt-fixture".into(),
                started_at_utc: "2026-10-20T00:00:00Z".into(),
                completed_at_utc: "2026-10-20T00:00:01Z".into(),
                status: "MATERIALIZED".into(),
                feature_set_id: None,
                diagnostic_code: None,
                diagnostic_message: None,
            },
        }
    }

    #[test]
    fn gate_feature_materialization_journal_registers_lineage_and_attempt_atomically() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let input = registered_lineage_dataset(&mut connection, 'a');
        let pending = pending_feature_set(vec![input.clone()]);
        save_pending_feature_set_promotion(&connection, &pending).unwrap();
        assert_eq!(
            list_pending_feature_set_promotions(&connection)
                .unwrap()
                .len(),
            1
        );
        assert!(
            get_feature_set_metadata(&connection, &pending.feature_set.feature_set_id)
                .unwrap()
                .is_none()
        );

        complete_pending_feature_set_promotion(
            &mut connection,
            &pending.feature_set.feature_set_id,
        )
        .unwrap();
        assert!(list_pending_feature_set_promotions(&connection)
            .unwrap()
            .is_empty());
        assert_eq!(
            get_feature_set_metadata(&connection, &pending.feature_set.feature_set_id).unwrap(),
            Some(pending.feature_set.clone())
        );
        assert_eq!(
            list_feature_set_inputs(&connection, &pending.feature_set.feature_set_id).unwrap(),
            vec![input]
        );
        let attempts = list_feature_attempts(&connection, 10).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "MATERIALIZED");
        assert_eq!(
            attempts[0].feature_set_id.as_deref(),
            Some(pending.feature_set.feature_set_id.as_str())
        );
    }

    #[test]
    fn gate_recovery_and_tamper_lineage_to_unregistered_dataset_rolls_back_and_keeps_journal() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let fingerprint = "9".repeat(64);
        let pending = pending_feature_set(vec![FeatureSetInput {
            dataset_id: format!("ds_{fingerprint}"),
            dataset_fingerprint: fingerprint,
        }]);
        save_pending_feature_set_promotion(&connection, &pending).unwrap();

        assert!(complete_pending_feature_set_promotion(
            &mut connection,
            &pending.feature_set.feature_set_id
        )
        .is_err());
        assert!(
            get_feature_set_metadata(&connection, &pending.feature_set.feature_set_id)
                .unwrap()
                .is_none()
        );
        assert!(list_feature_attempts(&connection, 10).unwrap().is_empty());
        assert_eq!(
            list_pending_feature_set_promotions(&connection)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn gate_recovery_and_tamper_feature_registry_rows_are_immutable_and_tamper_evident() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let input = registered_lineage_dataset(&mut connection, 'a');
        let pending = pending_feature_set(vec![input]);
        let id = pending.feature_set.feature_set_id.clone();
        save_pending_feature_set_promotion(&connection, &pending).unwrap();
        complete_pending_feature_set_promotion(&mut connection, &id).unwrap();

        for statement in [
            "UPDATE feature_sets SET status = 'BLOCKED'",
            "DELETE FROM feature_sets",
            "UPDATE feature_set_inputs SET dataset_fingerprint = dataset_fingerprint",
            "DELETE FROM feature_set_inputs",
            "UPDATE feature_attempts SET status = 'FAILED'",
            "DELETE FROM feature_attempts",
        ] {
            assert!(connection.execute(statement, []).is_err(), "{statement}");
        }
        connection
            .execute_batch(
                "DROP TRIGGER feature_sets_prevent_update;
                 UPDATE feature_sets SET manifest_json = manifest_json || ' ';",
            )
            .unwrap();
        assert!(
            !get_feature_set_metadata(&connection, &id)
                .unwrap()
                .unwrap()
                .manifest_integrity_valid
        );
    }

    #[test]
    fn gate_recovery_and_tamper_feature_journal_rejects_inconsistent_identity() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let input = registered_lineage_dataset(&mut connection, 'a');
        let mut wrong_id = pending_feature_set(vec![input.clone()]);
        wrong_id.feature_set.feature_set_id = format!("fs_{}", "0".repeat(64));
        assert!(save_pending_feature_set_promotion(&connection, &wrong_id).is_err());
        let mut wrong_manifest = pending_feature_set(vec![input.clone()]);
        wrong_manifest.feature_set.manifest_json.push(' ');
        assert!(save_pending_feature_set_promotion(&connection, &wrong_manifest).is_err());
        let mut duplicate_inputs = pending_feature_set(vec![input.clone(), input]);
        duplicate_inputs.staging_name = "fixture".into();
        assert!(save_pending_feature_set_promotion(&connection, &duplicate_inputs).is_err());
        assert!(list_pending_feature_set_promotions(&connection)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn gate_recovery_and_tamper_failed_feature_migration_rolls_back_to_dataset_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        let mut broken = MIGRATIONS[..5].to_vec();
        broken.push(Migration {
            version: 6,
            name: "intentional_feature_failure_fixture",
            sql: "CREATE TABLE feature_sets(id INTEGER); INSERT INTO missing_table VALUES (1);",
        });
        assert!(migrate_with(&mut connection, &broken, 6).is_err());
        assert_eq!(schema_version(&connection).unwrap(), 5);
        let feature_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'feature%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(feature_tables, 0);

        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        let feature_tables: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'feature%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(feature_tables, 4);
    }

    #[test]
    fn version_five_registry_upgrades_to_feature_schema_without_losing_datasets() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        assert_eq!(
            migrate_with(&mut connection, &MIGRATIONS[..5], 5).unwrap(),
            5
        );
        let input = registered_lineage_dataset(&mut connection, 'b');
        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        assert!(get_dataset_metadata(&connection, &input.dataset_id)
            .unwrap()
            .is_some());
        let applied: u32 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(applied, CURRENT_SCHEMA_VERSION);
    }

    /// Registers a READY feature set so decision lineage foreign keys have a real parent row.
    fn registered_feature_set(connection: &mut Connection) -> FeatureSetMetadata {
        let input = registered_lineage_dataset(connection, 'a');
        let pending = pending_feature_set(vec![input]);
        save_pending_feature_set_promotion(connection, &pending).unwrap();
        complete_pending_feature_set_promotion(connection, &pending.feature_set.feature_set_id)
            .unwrap();
        pending.feature_set
    }

    fn pending_decision_set(feature_set_id: &str) -> PendingDecisionSetPromotion {
        let identity = "d".repeat(64);
        let manifest_json = "{\"decision_fixture\":true}".to_owned();
        PendingDecisionSetPromotion {
            staging_name: "018f4c4f-7d9b-7b9f-a70e-92bb7cf04c63".into(),
            decision_set: DecisionSetMetadata {
                decision_set_id: format!("dc_{identity}"),
                identity_sha256: identity,
                feature_set_id: feature_set_id.into(),
                feature_set_manifest_sha256: "a".repeat(64),
                manifest_sha256: checksum(&manifest_json),
                manifest_json,
                output_sha256: "e".repeat(64),
                created_at_utc: "2026-10-20T00:00:02Z".into(),
                status: "READY".into(),
                manifest_integrity_valid: true,
            },
            attempt: DecisionAttemptRecord {
                attempt_id: "decision-attempt-fixture".into(),
                started_at_utc: "2026-10-20T00:00:01Z".into(),
                completed_at_utc: "2026-10-20T00:00:02Z".into(),
                status: "MATERIALIZED".into(),
                decision_set_id: None,
                diagnostic_code: None,
                diagnostic_message: None,
            },
        }
    }

    #[test]
    fn gate_immutable_decision_materialization_journal_registers_set_and_attempt_atomically() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let feature_set = registered_feature_set(&mut connection);
        let pending = pending_decision_set(&feature_set.feature_set_id);
        let id = pending.decision_set.decision_set_id.clone();
        save_pending_decision_set_promotion(&connection, &pending).unwrap();
        assert_eq!(
            list_pending_decision_set_promotions(&connection).unwrap(),
            vec![PendingDecisionSetPromotion {
                attempt: DecisionAttemptRecord {
                    decision_set_id: Some(id.clone()),
                    ..pending.attempt.clone()
                },
                ..pending.clone()
            }]
        );
        assert!(get_decision_set_metadata(&connection, &id)
            .unwrap()
            .is_none());

        complete_pending_decision_set_promotion(&mut connection, &id).unwrap();
        assert!(list_pending_decision_set_promotions(&connection)
            .unwrap()
            .is_empty());
        assert_eq!(
            get_decision_set_metadata(&connection, &id).unwrap(),
            Some(pending.decision_set.clone())
        );
        assert_eq!(
            list_decision_set_metadata_after(&connection, "", 10).unwrap(),
            vec![pending.decision_set.clone()]
        );
        let attempts = list_decision_attempts(&connection, 10).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "MATERIALIZED");
        assert_eq!(attempts[0].decision_set_id.as_deref(), Some(id.as_str()));

        save_pending_decision_set_promotion(&connection, &pending).unwrap();
        assert!(matches!(
            complete_pending_decision_set_promotion(&mut connection, &id),
            Err(PersistenceError::DecisionSetAlreadyExists(_))
        ));
    }

    #[test]
    fn gate_adversarial_tamper_decision_registry_rows_are_immutable_and_tamper_evident() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let feature_set = registered_feature_set(&mut connection);
        let pending = pending_decision_set(&feature_set.feature_set_id);
        let id = pending.decision_set.decision_set_id.clone();
        save_pending_decision_set_promotion(&connection, &pending).unwrap();
        complete_pending_decision_set_promotion(&mut connection, &id).unwrap();

        for statement in [
            "UPDATE decision_sets SET status = 'READY'",
            "DELETE FROM decision_sets",
            "UPDATE decision_attempts SET status = 'FAILED'",
            "DELETE FROM decision_attempts",
        ] {
            assert!(connection.execute(statement, []).is_err(), "{statement}");
        }
        connection
            .execute_batch(
                "DROP TRIGGER decision_sets_prevent_update;
                 UPDATE decision_sets SET manifest_json = manifest_json || ' ';",
            )
            .unwrap();
        assert!(
            !get_decision_set_metadata(&connection, &id)
                .unwrap()
                .unwrap()
                .manifest_integrity_valid
        );
    }

    #[test]
    fn gate_strategy_input_authority_decision_journal_rejects_inconsistent_identity_and_lineage() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        migrate(&mut connection).unwrap();
        let feature_set = registered_feature_set(&mut connection);
        let mut wrong_id = pending_decision_set(&feature_set.feature_set_id);
        wrong_id.decision_set.decision_set_id = format!("dc_{}", "0".repeat(64));
        let mut wrong_manifest = pending_decision_set(&feature_set.feature_set_id);
        wrong_manifest.decision_set.manifest_json.push(' ');
        let mut path_lineage = pending_decision_set(&feature_set.feature_set_id);
        path_lineage.decision_set.feature_set_id = "C:/MT5/Common/Files/EURUSD.csv".into();
        let mut blocked = pending_decision_set(&feature_set.feature_set_id);
        blocked.decision_set.status = "QUARANTINED".into();
        for candidate in [&wrong_id, &wrong_manifest, &path_lineage, &blocked] {
            assert!(save_pending_decision_set_promotion(&connection, candidate).is_err());
        }

        // Lineage to a feature set that was never registered fails the foreign key, rolls back,
        // and leaves the journal for fail-closed recovery.
        let unregistered = pending_decision_set(&format!("fs_{}", "9".repeat(64)));
        let id = unregistered.decision_set.decision_set_id.clone();
        save_pending_decision_set_promotion(&connection, &unregistered).unwrap();
        assert!(complete_pending_decision_set_promotion(&mut connection, &id).is_err());
        assert!(get_decision_set_metadata(&connection, &id)
            .unwrap()
            .is_none());
        assert!(list_decision_attempts(&connection, 10).unwrap().is_empty());
        assert_eq!(
            list_pending_decision_set_promotions(&connection)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn gate_resource_cancel_recovery_failed_decision_migration_rolls_back_to_feature_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        let mut broken = MIGRATIONS[..6].to_vec();
        broken.push(Migration {
            version: 7,
            name: "intentional_decision_failure_fixture",
            sql: "CREATE TABLE decision_sets(id INTEGER); INSERT INTO missing_table VALUES (1);",
        });
        assert!(migrate_with(&mut connection, &broken, 7).is_err());
        assert_eq!(schema_version(&connection).unwrap(), 6);
        let decision_tables = |connection: &Connection| -> u32 {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'decision%'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(decision_tables(&connection), 0);

        assert_eq!(migrate(&mut connection).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(decision_tables(&connection), 3);
    }

    /// A schema 6 store, as the accepted RG-R02 application leaves it, holding a dataset and a
    /// feature set. The returned writer stays open so its committed rows remain in the WAL file
    /// rather than being checkpointed into the main database file.
    fn populated_v6_store(directory: &Path) -> (std::path::PathBuf, Connection) {
        let path = directory.join("research.sqlite3");
        let mut writer = Connection::open(&path).unwrap();
        configure(&writer).unwrap();
        writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        assert_eq!(migrate_with(&mut writer, &MIGRATIONS[..6], 6).unwrap(), 6);
        registered_feature_set(&mut writer);
        let wal = directory.join("research.sqlite3-wal");
        assert!(
            std::fs::metadata(&wal).unwrap().len() > 0,
            "fixture rows must be in the WAL"
        );
        (path, writer)
    }

    fn row_count(connection: &Connection, table: &str) -> u32 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn gate_resource_cancel_recovery_schema_upgrade_backs_up_populated_v6_with_wal_content() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, _writer) = populated_v6_store(directory.path());
        let upgraded = open_database(&path).unwrap();
        assert_eq!(schema_version(&upgraded).unwrap(), CURRENT_SCHEMA_VERSION);

        let backups: Vec<_> = std::fs::read_dir(directory.path().join("schema-backups"))
            .expect("a schema 6 store must be backed up before it is upgraded")
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "sqlite3")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        // Every backup is anchored by a receipt written from the source's own digest.
        assert!(schema_backup_receipt_path(&backups[0]).exists());
        assert_eq!(
            verify_schema_backup(&path, &backups[0])
                .unwrap()
                .schema_version,
            6
        );
        let backup =
            Connection::open_with_flags(&backups[0], rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        assert_eq!(schema_version(&backup).unwrap(), 6);
        // Rows that existed only in the WAL at upgrade time are in the backup.
        assert_eq!(row_count(&backup, "datasets"), 1);
        assert_eq!(row_count(&backup, "feature_sets"), 1);
        assert_eq!(row_count(&backup, "feature_set_inputs"), 1);
    }

    #[test]
    fn gate_resource_cancel_recovery_schema_upgrade_fails_closed_when_backup_cannot_be_written() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, writer) = populated_v6_store(directory.path());
        // A regular file where the backup directory belongs makes the backup impossible.
        std::fs::write(directory.path().join("schema-backups"), b"not a directory").unwrap();
        assert!(open_database(&path).is_err());
        assert_eq!(
            schema_version(&writer).unwrap(),
            6,
            "the store must not be upgraded"
        );
        assert_eq!(row_count(&writer, "datasets"), 1);
        assert_eq!(row_count(&writer, "feature_sets"), 1);
    }

    /// Opens the store as the accepted RG-R02 build would: it knows migrations 1-6 only.
    fn open_as_schema_six_build(path: &Path) -> Result<Connection, PersistenceError> {
        open_database_with(path, &MIGRATIONS[..6], 6)
    }

    #[test]
    fn gate_resource_cancel_recovery_schema_upgrade_restores_v6_for_the_prior_build_unchanged() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, writer) = populated_v6_store(directory.path());
        let v6_digest = database_content_sha256(&writer).unwrap();
        let dataset_id = format!("ds_{}", "a".repeat(64));
        let feature_set_id = format!("fs_{}", "f".repeat(64));
        let dataset = get_dataset_metadata(&writer, &dataset_id).unwrap();
        let feature_set = get_feature_set_metadata(&writer, &feature_set_id).unwrap();
        drop(writer);

        // Upgrading adds schema 7 without touching existing dataset or feature lineage.
        let upgraded = open_database(&path).unwrap();
        assert_eq!(
            get_dataset_metadata(&upgraded, &dataset_id).unwrap(),
            dataset
        );
        assert_eq!(
            get_feature_set_metadata(&upgraded, &feature_set_id).unwrap(),
            feature_set
        );
        drop(upgraded);
        assert!(matches!(
            open_as_schema_six_build(&path),
            Err(PersistenceError::FutureSchema {
                found: 7,
                supported: 6
            })
        ));

        let backups = list_schema_backups(&path).unwrap();
        assert_eq!(backups.len(), 1);
        let restored = restore_schema_backup(&path, &backups[0]).unwrap();
        assert_eq!(restored.schema_version, 6);
        assert_eq!(restored.content_sha256, v6_digest);
        assert!(restored.preserved.iter().all(|moved| moved.exists()));
        assert!(
            !restored.preserved.is_empty(),
            "the upgraded files are kept, not deleted"
        );

        let reopened = open_as_schema_six_build(&path).unwrap();
        assert_eq!(schema_version(&reopened).unwrap(), 6);
        assert_eq!(database_content_sha256(&reopened).unwrap(), v6_digest);
        assert_eq!(
            get_dataset_metadata(&reopened, &dataset_id).unwrap(),
            dataset
        );
        assert_eq!(
            get_feature_set_metadata(&reopened, &feature_set_id).unwrap(),
            feature_set
        );
    }

    #[test]
    fn gate_resource_cancel_recovery_failed_schema_upgrade_keeps_v6_and_a_verified_backup() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, writer) = populated_v6_store(directory.path());
        let v6_digest = database_content_sha256(&writer).unwrap();
        let mut broken = MIGRATIONS[..6].to_vec();
        broken.push(Migration {
            version: 7,
            name: "intentional_upgrade_failure_fixture",
            sql: "CREATE TABLE decision_sets(id INTEGER); INSERT INTO missing_table VALUES (1);",
        });
        assert!(open_database_with(&path, &broken, 7).is_err());
        assert_eq!(schema_version(&writer).unwrap(), 6);
        assert_eq!(database_content_sha256(&writer).unwrap(), v6_digest);
        let backups = list_schema_backups(&path).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(verified_snapshot(&backups[0]).unwrap(), (6, v6_digest));
        drop(writer);
        assert!(open_as_schema_six_build(&path).is_ok());
    }

    #[test]
    fn gate_resource_cancel_recovery_schema_restore_refuses_unsafe_requests() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, writer) = populated_v6_store(directory.path());
        drop(writer);
        drop(open_database(&path).unwrap());
        let backup = list_schema_backups(&path).unwrap().remove(0);

        // In use: a reader holding a transaction keeps a full checkpoint from completing.
        let reader = Connection::open(&path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM datasets;")
            .unwrap();
        let in_use = restore_schema_backup(&path, &backup);
        assert!(
            matches!(&in_use, Err(PersistenceError::SchemaRestoreFailed(reason)) if reason.contains("in use")),
            "{in_use:?}"
        );
        reader.execute_batch("COMMIT").unwrap();
        drop(reader);

        // Outside the backup directory, and a corrupted backup.
        let outside = directory.path().join("copied-backup.sqlite3");
        std::fs::copy(&backup, &outside).unwrap();
        assert!(restore_schema_backup(&path, &outside).is_err());
        let corrupted = backup.with_file_name("research.sqlite3.schema-v6.corrupted.sqlite3");
        let mut bytes = std::fs::read(&backup).unwrap();
        for byte in bytes.iter_mut().skip(100).take(4000) {
            *byte ^= 0x5a;
        }
        std::fs::write(&corrupted, bytes).unwrap();
        assert!(restore_schema_backup(&path, &corrupted).is_err());

        // Every refusal left the upgraded store in place.
        assert_eq!(
            schema_version(&open_database(&path).unwrap()).unwrap(),
            CURRENT_SCHEMA_VERSION
        );
    }

    /// Changes a backup with ordinary SQL so it stays a structurally valid SQLite database.
    fn alter_backup_logically(backup: &Path) {
        let connection = Connection::open(backup).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER datasets_prevent_update;
                 UPDATE datasets SET status = 'BLOCKED';",
            )
            .unwrap();
        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            integrity, "ok",
            "the altered backup must still be valid SQLite"
        );
    }

    #[test]
    fn gate_adversarial_tamper_schema_restore_rejects_a_logically_altered_valid_backup() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, writer) = populated_v6_store(directory.path());
        drop(writer);
        let upgraded = open_database(&path).unwrap();
        let live_digest = database_content_sha256(&upgraded).unwrap();
        drop(upgraded);
        let backup = list_schema_backups(&path).unwrap().remove(0);
        alter_backup_logically(&backup);

        assert!(
            restore_schema_backup(&path, &backup).is_err(),
            "a backup whose content no longer matches what was backed up must not be restored"
        );
        assert!(matches!(
            verify_schema_backup(&path, &backup),
            Err(PersistenceError::SchemaBackupUnverified(_))
        ));
        let live = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&live).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(database_content_sha256(&live).unwrap(), live_digest);
    }

    /// An upgraded store with one receipted backup; returns the store path, backup, and the live
    /// schema 7 digest.
    fn upgraded_with_backup(directory: &Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let (path, writer) = populated_v6_store(directory);
        drop(writer);
        let upgraded = open_database(&path).unwrap();
        let live_digest = database_content_sha256(&upgraded).unwrap();
        drop(upgraded);
        let backup = list_schema_backups(&path).unwrap().remove(0);
        (path, backup, live_digest)
    }

    fn assert_live_unchanged(path: &Path, live_digest: &str) {
        let live = Connection::open(path).unwrap();
        assert_eq!(schema_version(&live).unwrap(), CURRENT_SCHEMA_VERSION);
        assert_eq!(database_content_sha256(&live).unwrap(), live_digest);
    }

    #[test]
    fn gate_adversarial_tamper_schema_restore_requires_a_well_formed_matching_receipt() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, backup, live_digest) = upgraded_with_backup(directory.path());
        let receipt_path = schema_backup_receipt_path(&backup);
        let receipt = std::fs::read_to_string(&receipt_path).unwrap();
        let parsed: SchemaBackupReceipt = serde_json::from_str(&receipt).unwrap();
        assert_eq!(parsed.schema_version, 6);
        assert_eq!(parsed.source_database_file_name, "research.sqlite3");

        let mut wrong_name = parsed.clone();
        wrong_name.backup_file_name = "another-backup.sqlite3".into();
        let mut wrong_digest = parsed.clone();
        wrong_digest.content_sha256 = "0".repeat(64);
        let cases = [
            ("missing", None),
            (
                "malformed",
                Some("{\"receipt_version\": 1, \"truncated".to_owned()),
            ),
            (
                "unknown field",
                Some(receipt.replacen('{', "{\"trusted\": true,", 1)),
            ),
            (
                "other backup",
                Some(serde_json::to_string(&wrong_name).unwrap()),
            ),
            (
                "other digest",
                Some(serde_json::to_string(&wrong_digest).unwrap()),
            ),
        ];
        for (case, content) in cases {
            match &content {
                None => std::fs::remove_file(&receipt_path).unwrap(),
                Some(text) => std::fs::write(&receipt_path, text).unwrap(),
            }
            assert!(
                matches!(
                    verify_schema_backup(&path, &backup),
                    Err(PersistenceError::SchemaBackupUnverified(_))
                ),
                "{case}"
            );
            assert!(restore_schema_backup(&path, &backup).is_err(), "{case}");
            assert_live_unchanged(&path, &live_digest);
        }

        // With its original receipt back, the same backup verifies and restores.
        std::fs::write(&receipt_path, &receipt).unwrap();
        assert_eq!(
            restore_schema_backup(&path, &backup)
                .unwrap()
                .content_sha256,
            parsed.content_sha256
        );
    }

    #[test]
    fn gate_resource_cancel_recovery_failed_final_restore_check_puts_the_original_back() {
        let directory = tempfile::TempDir::new().unwrap();
        let (path, backup, live_digest) = upgraded_with_backup(directory.path());
        FAIL_INSTALLED_VERIFICATION.with(|fail| fail.set(true));
        let result = restore_schema_backup(&path, &backup);
        FAIL_INSTALLED_VERIFICATION.with(|fail| fail.set(false));
        let reason = match result {
            Err(PersistenceError::SchemaRestoreFailed(reason)) => reason,
            other => panic!("a restore that failed its final check reported {other:?}"),
        };
        assert!(
            reason.contains("original database was put back"),
            "{reason}"
        );
        assert_live_unchanged(&path, &live_digest);
        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            leftovers
                .iter()
                .any(|name| name.contains(".failed-restore-")),
            "{leftovers:?}"
        );
        assert!(
            !leftovers.iter().any(|name| name.contains(".pre-restore-")),
            "{leftovers:?}"
        );
        assert!(
            !leftovers.iter().any(|name| name.ends_with(".tmp")),
            "{leftovers:?}"
        );
    }

    #[test]
    fn gate_resource_cancel_recovery_schema_backup_is_only_taken_for_an_existing_older_store() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("research.sqlite3");
        drop(open_database(&path).unwrap());
        drop(open_database(&path).unwrap());
        assert!(list_schema_backups(&path).unwrap().is_empty());
        assert!(!directory.path().join(SCHEMA_BACKUP_DIRECTORY).exists());
    }
}
