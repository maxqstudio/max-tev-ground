use std::path::Path;

use rg_contracts::ExperimentManifest;
use rusqlite::{Connection, TransactionBehavior};
use thiserror::Error;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

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
        assert_eq!(count, 2);
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
            assert_eq!(migrations, 2);
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
}
