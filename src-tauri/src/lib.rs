mod commands {
    use rg_governance::GovernanceSnapshot;

    #[tauri::command]
    pub fn get_foundation_status() -> Result<GovernanceSnapshot, String> {
        rg_governance::validate_embedded()
    }
}

fn register_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder.invoke_handler(tauri::generate_handler![commands::get_foundation_status])
}

pub fn run() {
    register_commands(tauri::Builder::default())
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

        assert_eq!(response["current_phase"], "RG-R01");
        assert_eq!(response["phase_lifecycle"], "ACTIVE");
        assert_eq!(response["pages"].as_array().unwrap().len(), 8);
        assert_eq!(
            response["accepted_phases"][0]["owner_ui_status"],
            "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED"
        );
        assert_eq!(response["pages"][1]["status"], "IN_PROGRESS");
        assert!(!response["next_phase_unlocked"].as_bool().unwrap());
    }

    #[test]
    fn direct_command_boundary_never_claims_owner_acceptance() {
        let response = commands::get_foundation_status().unwrap();
        assert_eq!(response.current_phase, "RG-R01");
        assert_eq!(
            response.owner_acceptance,
            rg_governance::OwnerAcceptanceStatus::Pending
        );
        assert!(!response.next_phase_unlocked);
    }
}
