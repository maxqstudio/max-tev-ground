//! Lists and restores the verified pre-upgrade backups that `open_database` writes before a schema
//! upgrade. Run with the desktop application closed.
//!
//!   rg-schema-backup list <database>
//!   rg-schema-backup restore <database> <backup>

use std::{env, path::PathBuf, process::ExitCode};

use rg_persistence::{list_schema_backups, restore_schema_backup, verify_schema_backup};

fn main() -> ExitCode {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let result = match arguments.as_slice() {
        // Each backup is listed with its receipt check, so an unverifiable one is never offered
        // as restorable.
        [command, database] if command == "list" => {
            let database = PathBuf::from(database);
            list_schema_backups(&database).map(|backups| {
                for backup in &backups {
                    match verify_schema_backup(&database, backup) {
                        Ok(verified) => println!(
                            "VERIFIED schema-v{} {}",
                            verified.schema_version,
                            backup.display()
                        ),
                        Err(error) => println!("UNVERIFIED {} ({error})", backup.display()),
                    }
                }
                println!("SCHEMA_BACKUPS={}", backups.len());
            })
        }
        [command, database, backup] if command == "restore" => {
            restore_schema_backup(&PathBuf::from(database), &PathBuf::from(backup)).map(
                |restored| {
                    for moved in &restored.preserved {
                        println!("PRESERVED={}", moved.display());
                    }
                    println!("RESTORED_SCHEMA_VERSION={}", restored.schema_version);
                    println!("RESTORED_CONTENT_SHA256={}", restored.content_sha256);
                },
            )
        }
        _ => {
            eprintln!(
                "usage: rg-schema-backup list <database>\n       rg-schema-backup restore <database> <backup>"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("SCHEMA_BACKUP=FAIL {error}");
            ExitCode::FAILURE
        }
    }
}
