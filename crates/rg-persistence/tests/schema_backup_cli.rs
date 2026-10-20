//! Drives the `rg-schema-backup` binary as an Owner would: list the pre-upgrade backup of a
//! schema 6 store, restore it, and confirm the schema 6 content is back.

use std::process::Command;

use rg_persistence::{
    configure, database_content_sha256, migrate_to_version_for_tests, open_database,
    save_dataset_and_attempt, schema_version, DatasetMetadata, ImportAttemptRecord,
};
use rusqlite::Connection;

fn schema_six_store(path: &std::path::Path) -> String {
    let mut connection = Connection::open(path).unwrap();
    configure(&connection).unwrap();
    assert_eq!(migrate_to_version_for_tests(&mut connection, 6).unwrap(), 6);
    let fingerprint = "a".repeat(64);
    let manifest_json = "{\"fixture\":\"synthetic-fixture:schema-backup-cli\"}".to_owned();
    let manifest_sha256 = rg_contracts::Sha256Digest::of_bytes(manifest_json.as_bytes())
        .as_str()
        .to_owned();
    save_dataset_and_attempt(
        &mut connection,
        &DatasetMetadata {
            dataset_id: format!("ds_{fingerprint}"),
            fingerprint,
            manifest_json,
            manifest_sha256,
            parquet_sha256: "b".repeat(64),
            raw_sha256: "c".repeat(64),
            created_at_utc: "2026-10-20T00:00:00Z".into(),
            status: "READY".into(),
            manifest_integrity_valid: true,
        },
        &ImportAttemptRecord {
            attempt_id: "schema-backup-cli".into(),
            started_at_utc: "2026-10-20T00:00:00Z".into(),
            completed_at_utc: "2026-10-20T00:00:00Z".into(),
            status: "IMPORTED".into(),
            source_file_name: "synthetic-fixture.csv".into(),
            diagnostic_code: None,
            diagnostic_message: None,
        },
    )
    .unwrap();
    database_content_sha256(&connection).unwrap()
}

fn run(arguments: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_rg-schema-backup"))
        .args(arguments)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn gate_resource_cancel_recovery_schema_backup_cli_lists_and_restores_a_v6_store() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = directory.path().join("research.sqlite3");
    let v6_digest = schema_six_store(&path);
    drop(open_database(&path).unwrap());
    let database = path.to_str().unwrap();

    let (listed, text) = run(&["list", database]);
    assert!(listed, "{text}");
    assert!(text.contains("SCHEMA_BACKUPS=1"), "{text}");
    let backup = text
        .lines()
        .find(|line| line.ends_with(".sqlite3") && line.contains("schema-v6"))
        .unwrap()
        .to_owned();

    let (restored, text) = run(&["restore", database, &backup]);
    assert!(restored, "{text}");
    assert!(text.contains("RESTORED_SCHEMA_VERSION=6"), "{text}");
    assert!(
        text.contains(&format!("RESTORED_CONTENT_SHA256={v6_digest}")),
        "{text}"
    );
    let reopened = Connection::open(&path).unwrap();
    assert_eq!(schema_version(&reopened).unwrap(), 6);
    assert_eq!(database_content_sha256(&reopened).unwrap(), v6_digest);

    let (usage, text) = run(&["delete", database]);
    assert!(!usage, "{text}");
}
