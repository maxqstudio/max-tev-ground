mod data_ground_commands;
mod feature_studio_commands;
mod strategy_studio_commands;

use tauri::Manager;

mod commands {
    use rg_governance::GovernanceSnapshot;

    #[tauri::command]
    pub fn get_foundation_status() -> Result<GovernanceSnapshot, String> {
        rg_governance::validate_embedded()
    }
}

fn register_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![
        commands::get_foundation_status,
        data_ground_commands::select_data_source,
        data_ground_commands::inspect_selected_source,
        data_ground_commands::preview_data_import,
        data_ground_commands::get_active_data_import,
        data_ground_commands::confirm_data_import,
        data_ground_commands::cancel_data_import,
        data_ground_commands::start_dataset_catalog_verification,
        data_ground_commands::get_dataset_catalog_progress,
        data_ground_commands::cancel_dataset_catalog_verification,
        data_ground_commands::list_datasets,
        data_ground_commands::inspect_dataset,
        data_ground_commands::list_import_attempts,
        feature_studio_commands::get_feature_registry,
        feature_studio_commands::list_feature_inputs,
        feature_studio_commands::preview_feature_set,
        feature_studio_commands::get_active_feature_job,
        feature_studio_commands::confirm_feature_set,
        feature_studio_commands::cancel_feature_job,
        feature_studio_commands::list_feature_sets,
        feature_studio_commands::inspect_feature_set,
        feature_studio_commands::list_feature_attempts,
        strategy_studio_commands::get_strategy_registry,
        strategy_studio_commands::list_decision_inputs,
        strategy_studio_commands::preview_decision_set,
        strategy_studio_commands::get_active_decision_job,
        strategy_studio_commands::confirm_decision_set,
        strategy_studio_commands::cancel_decision_job,
        strategy_studio_commands::list_decision_sets,
        strategy_studio_commands::inspect_decision_set,
        strategy_studio_commands::list_decision_attempts
    ])
}

pub fn run() {
    register_commands(tauri::Builder::default())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_directory = app.path().app_data_dir()?;
            let data_ground = data_ground_commands::DataGroundState::open(&data_directory)?;
            let feature_studio = feature_studio_commands::FeatureStudioState::open(
                &data_directory,
                data_ground.store(),
            )?;
            let strategy_studio = strategy_studio_commands::StrategyStudioState::open(
                &data_directory,
                feature_studio.store(),
            )?;
            app.manage(data_ground);
            app.manage(feature_studio);
            app.manage(strategy_studio);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run MAX Research Ground desktop shell");
}

#[cfg(test)]
mod tests {
    use super::{commands, register_commands};
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{mock_builder, mock_context, noop_assets, INVOKE_KEY},
        webview::{InvokeRequest, WebviewWindowBuilder},
    };

    #[test]
    fn foundation_status_command_is_registered_and_returns_governance_contract() {
        let app = register_commands(mock_builder())
            .build(mock_context(noop_assets()))
            .expect("mock Tauri runtime should build");
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview should build");
        let response = tauri::test::get_ipc_response(
            &webview,
            InvokeRequest {
                cmd: "get_foundation_status".into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "http://tauri.localhost".parse().unwrap(),
                body: InvokeBody::default(),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .expect("registered command should answer IPC")
        .deserialize::<serde_json::Value>()
        .expect("command response should be JSON");

        assert_eq!(response["current_phase"], "RG-R04");
        assert_eq!(response["phase_lifecycle"], "ACTIVE");
        assert_eq!(response["builder_status"], "NOT_STARTED");
        assert_eq!(response["automated_status"], "NOT_RUN");
        assert_eq!(response["owner_ui_status"], "PENDING");
        assert_eq!(response["owner_acceptance"], "PENDING");
        assert_eq!(response["pages"].as_array().unwrap().len(), 8);
        assert_eq!(
            response["accepted_phases"][1]["source_tree_sha"],
            "46c8969bc31faa7f3efaa6474d5ced2322270623"
        );
        assert_eq!(response["accepted_phases"][2]["phase"], "RG-R02");
        assert_eq!(
            response["accepted_phases"][2]["owner_ui_status"],
            "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED"
        );
        assert_eq!(
            response["accepted_phases"][2]["source_tree_sha"],
            "326b01e72b915746be2134d00a2d0d1fa737d853"
        );
        assert_eq!(response["accepted_phases"][3]["phase"], "RG-R03");
        assert_eq!(
            response["accepted_phases"][3]["owner_ui_status"],
            "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED"
        );
        assert_eq!(
            response["accepted_phases"][3]["source_tree_sha"],
            "4e8f1542a1d6a3872219c1dad9747dde5841bd88"
        );
        assert_eq!(response["pages"][1]["status"], "OWNER_ACCEPTED");
        assert_eq!(response["pages"][2]["status"], "OWNER_ACCEPTED");
        assert_eq!(response["pages"][3]["status"], "OWNER_ACCEPTED");
        assert_eq!(response["pages"][4]["status"], "ACTIVE_NOT_STARTED");
        assert_eq!(response["pages"][5]["status"], "PLANNED");
        assert!(!response["next_phase_unlocked"].as_bool().unwrap());
    }

    #[test]
    fn direct_command_boundary_never_claims_owner_acceptance() {
        let response = commands::get_foundation_status().unwrap();
        assert_eq!(response.current_phase, "RG-R04");
        assert_eq!(
            response.builder_status,
            rg_governance::BuilderStatus::NotStarted
        );
        assert_eq!(
            response.automated_status,
            rg_governance::AutomatedStatus::NotRun
        );
        assert_eq!(
            response.owner_ui_status,
            rg_governance::OwnerUiStatus::Pending
        );
        assert_eq!(
            response.owner_acceptance,
            rg_governance::OwnerAcceptanceStatus::Pending
        );
        assert!(!response.next_phase_unlocked);
    }
}
