use std::{env, path::PathBuf, process::ExitCode};

use rg_governance::{validate_project, write_roadmap_document};

fn main() -> ExitCode {
    let argument = env::args().nth(1).unwrap_or_else(|| "--check".into());
    let root = env::current_dir().unwrap_or_else(|error| {
        eprintln!("cannot resolve project root: {error}");
        PathBuf::new()
    });
    match argument.as_str() {
        "--check" => match validate_project(&root) {
            Ok(snapshot) => {
                println!(
                    "ROADMAP_GOVERNANCE=PASS phase={} lifecycle={:?} deliverables={}",
                    snapshot.current_phase,
                    snapshot.phase_lifecycle,
                    snapshot.deliverables.len()
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("ROADMAP_GOVERNANCE=FAIL {error}");
                ExitCode::FAILURE
            }
        },
        "--write" => match write_roadmap_document(&root) {
            Ok(()) => {
                println!("ROADMAP_DOCUMENT=GENERATED");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("ROADMAP_DOCUMENT=FAIL {error}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("unsupported argument: {other}; use --check or --write");
            ExitCode::FAILURE
        }
    }
}
