use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

const ROADMAP_SOURCE: &str = include_str!("../../../ROADMAP.yaml");
const PHASE_SOURCE: &str = include_str!("../../../PHASE_MANIFEST.yaml");
const IMPLEMENTATION_SOURCE: &str = include_str!("../../../IMPLEMENTATION_STATE.yaml");
const REQUIRED_RG_R00_GATES: [&str; 9] = [
    "rust_workspace",
    "frontend",
    "tauri_build",
    "persistence",
    "contracts",
    "roadmap_governance",
    "baseline_read_only",
    "exact_source_evidence",
    "owner_ui_acceptance",
];
const REQUIRED_RG_R01_GATES: [&str; 13] = [
    "csv_ingestion",
    "parquet_ingestion",
    "multi_market_timeframe",
    "quality_audit",
    "temporal_contract",
    "snapshot_integrity",
    "dataset_registry",
    "atomicity_and_cancel",
    "path_safety",
    "data_ground_ipc_ui",
    "persistence_migrations",
    "reproducible_acceptance",
    "owner_ui_acceptance",
];
const REQUIRED_RG_R02_GATES: [&str; 13] = [
    "input_authority",
    "indicator_registry",
    "causal_availability",
    "multi_symbol_timeframe_join",
    "feature_materialization",
    "resource_controls",
    "feature_studio_ui",
    "data_policy",
    "golden_vector_math",
    "adversarial_no_lookahead",
    "recovery_and_tamper",
    "reproducible_acceptance",
    "owner_ui_acceptance",
];
const REQUIRED_RG_R03_GATES: [&str; 12] = [
    "strategy_input_authority",
    "typed_strategy_registry",
    "decision_semantics",
    "causal_no_lookahead",
    "mtf_context_only",
    "skip_and_conflict_handling",
    "immutable_decision_materialization",
    "resource_cancel_recovery",
    "desktop_strategy_ipc",
    "adversarial_tamper",
    "reproducible_acceptance",
    "owner_ui_acceptance",
];
const REQUIRED_RG_R04_GATES: [&str; 13] = [
    "local_runtime_boundary",
    "model_discovery",
    "immutable_model_identity",
    "typed_runtime_adapters",
    "verified_model_input",
    "model_authority_boundary",
    "inference_materialization",
    "model_comparison",
    "resource_cancel_recovery",
    "desktop_model_lab_ipc",
    "adversarial_tamper",
    "reproducible_acceptance",
    "owner_ui_acceptance",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roadmap {
    pub schema_version: u16,
    pub project: String,
    pub current_phase: String,
    pub phases: Vec<RoadmapPhase>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoadmapPhase {
    pub id: String,
    pub name: String,
    pub objective: String,
    pub lifecycle: Lifecycle,
    pub acceptance_evidence: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Lifecycle {
    Active,
    Planned,
    Accepted,
}

impl Lifecycle {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Planned => "PLANNED",
            Self::Accepted => "ACCEPTED",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseManifest {
    pub schema_version: u16,
    pub phase: String,
    pub lifecycle: Lifecycle,
    pub deliverables: Vec<String>,
    pub acceptance_gates: Vec<AcceptanceGate>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceGate {
    pub id: String,
    pub requirement: String,
    pub evidence: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationState {
    pub schema_version: u16,
    pub current_phase: String,
    pub builder_status: BuilderStatus,
    pub automated_status: AutomatedStatus,
    pub owner_ui_status: OwnerUiStatus,
    pub owner_acceptance: OwnerAcceptanceStatus,
    pub next_phase_unlocked: bool,
    pub deliverables: BTreeMap<String, DeliveryStatus>,
    #[serde(default)]
    pub accepted_phases: Vec<PhaseAcceptanceRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PhaseAcceptanceRecord {
    pub phase: String,
    pub owner_acceptance: OwnerAcceptanceStatus,
    pub owner_ui_status: OwnerUiStatus,
    pub source_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tree_sha: Option<String>,
    pub ci_run_url: String,
    pub owner_issue_url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_debts: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OwnerAcceptanceStatus {
    Pending,
    OwnerAccepted,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BuilderStatus {
    NotStarted,
    InProgress,
    Implemented,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AutomatedStatus {
    NotRun,
    AutomatedPass,
    EnvironmentBlocked,
    Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OwnerUiStatus {
    Pending,
    OwnerAccepted,
    OwnerAcceptedWithUiReviewWaived,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeliveryStatus {
    Planned,
    Implemented,
    Tested,
    Verified,
    Blocked,
    OwnerAccepted,
}

#[derive(Clone, Debug, Serialize)]
pub struct GovernanceSnapshot {
    pub project: String,
    pub current_phase: String,
    pub phase_lifecycle: Lifecycle,
    pub builder_status: BuilderStatus,
    pub automated_status: AutomatedStatus,
    pub owner_ui_status: OwnerUiStatus,
    pub owner_acceptance: OwnerAcceptanceStatus,
    pub next_phase_unlocked: bool,
    pub deliverables: BTreeMap<String, DeliveryStatus>,
    pub accepted_phases: Vec<PhaseAcceptanceRecord>,
    pub pages: Vec<PageSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageSnapshot {
    pub id: &'static str,
    pub status: String,
}

#[derive(Debug)]
struct Documents {
    roadmap: Roadmap,
    implementation: ImplementationState,
}

pub fn validate_embedded() -> Result<GovernanceSnapshot, String> {
    let documents = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, IMPLEMENTATION_SOURCE)?;
    snapshot(documents)
}

pub fn validate_project(root: impl AsRef<Path>) -> Result<GovernanceSnapshot, String> {
    let root = root.as_ref();
    let roadmap = fs::read_to_string(root.join("ROADMAP.yaml")).map_err(|e| e.to_string())?;
    let phase = fs::read_to_string(root.join("PHASE_MANIFEST.yaml")).map_err(|e| e.to_string())?;
    let implementation =
        fs::read_to_string(root.join("IMPLEMENTATION_STATE.yaml")).map_err(|e| e.to_string())?;
    let documents = parse_documents(&roadmap, &phase, &implementation)?;
    let expected = render_roadmap(&documents.roadmap);
    let actual = fs::read_to_string(root.join("ROADMAP.md")).map_err(|e| e.to_string())?;
    if actual != expected {
        return Err("ROADMAP.md is out of sync with ROADMAP.yaml".into());
    }
    snapshot(documents)
}

pub fn render_roadmap(roadmap: &Roadmap) -> String {
    let mut output = format!(
        "# MAX Research Ground roadmap\n\nGenerated from ROADMAP.yaml. Current active phase: {}. Later phases stay locked until the Owner accepts the current checkpoint and authorizes progression.\n\n",
        roadmap.current_phase
    );
    for phase in &roadmap.phases {
        output.push_str(&format!(
            "## {} — {} ({})\n\n{}\n\n",
            phase.id,
            phase.name,
            phase.lifecycle.as_str(),
            phase.objective
        ));
        if let Some(evidence) = &phase.acceptance_evidence {
            output.push_str(&format!("Acceptance evidence: {evidence}\n\n"));
        }
    }
    output.truncate(output.trim_end().len());
    output.push('\n');
    output
}

fn parse_documents(
    roadmap_source: &str,
    phase_source: &str,
    implementation_source: &str,
) -> Result<Documents, String> {
    let roadmap: Roadmap = serde_yaml_ng::from_str(roadmap_source).map_err(|e| e.to_string())?;
    let phase: PhaseManifest = serde_yaml_ng::from_str(phase_source).map_err(|e| e.to_string())?;
    let implementation: ImplementationState =
        serde_yaml_ng::from_str(implementation_source).map_err(|e| e.to_string())?;
    validate_sync(&roadmap, &phase, &implementation)?;
    Ok(Documents {
        roadmap,
        implementation,
    })
}

fn validate_sync(
    roadmap: &Roadmap,
    phase: &PhaseManifest,
    implementation: &ImplementationState,
) -> Result<(), String> {
    if roadmap.schema_version != 1
        || phase.schema_version != 3
        || implementation.schema_version != 3
    {
        return Err("unsupported governance schema version".into());
    }
    if roadmap.phases.len() != 9 {
        return Err("roadmap must contain exactly RG-R00 through RG-R08".into());
    }
    for (index, item) in roadmap.phases.iter().enumerate() {
        let expected = format!("RG-R{index:02}");
        if item.id != expected || item.name.trim().is_empty() || item.objective.trim().is_empty() {
            return Err(format!(
                "invalid or out-of-order roadmap phase: {}",
                item.id
            ));
        }
        if item.lifecycle == Lifecycle::Accepted
            && item
                .acceptance_evidence
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
        {
            return Err(format!("accepted phase {} has no evidence", item.id));
        }
        if item.lifecycle != Lifecycle::Accepted && item.acceptance_evidence.is_some() {
            return Err(format!(
                "unaccepted phase {} has acceptance evidence",
                item.id
            ));
        }
    }
    let active: Vec<_> = roadmap
        .phases
        .iter()
        .filter(|item| item.lifecycle == Lifecycle::Active)
        .collect();
    if active.len() != 1 || active[0].id != roadmap.current_phase {
        return Err("roadmap must have exactly one active phase matching current_phase".into());
    }
    let active_index = roadmap
        .phases
        .iter()
        .position(|item| item.id == roadmap.current_phase)
        .ok_or_else(|| "current_phase is absent from roadmap".to_owned())?;
    if roadmap.phases[..active_index]
        .iter()
        .any(|item| item.lifecycle != Lifecycle::Accepted)
    {
        return Err("a previous phase is not accepted with evidence".into());
    }
    if roadmap.phases[active_index + 1..]
        .iter()
        .any(|item| item.lifecycle != Lifecycle::Planned)
    {
        return Err("a later phase has been activated before the current phase".into());
    }
    validate_acceptance_history(roadmap, implementation, active_index)?;
    if phase.phase != roadmap.current_phase || implementation.current_phase != roadmap.current_phase
    {
        return Err(
            "roadmap, phase manifest, and implementation state disagree on current phase".into(),
        );
    }
    if phase.lifecycle != active[0].lifecycle {
        return Err("phase manifest lifecycle is out of sync with roadmap".into());
    }
    if phase.deliverables.is_empty() {
        return Err("phase manifest must declare deliverables".into());
    }
    if phase
        .deliverables
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != phase.deliverables.len()
    {
        return Err("phase manifest contains duplicate deliverables".into());
    }
    validate_acceptance_gates(phase)?;
    let declared: std::collections::BTreeSet<_> = phase.deliverables.iter().cloned().collect();
    let recorded: std::collections::BTreeSet<_> =
        implementation.deliverables.keys().cloned().collect();
    if declared != recorded {
        return Err("phase manifest and implementation state deliverables are out of sync".into());
    }
    if implementation.owner_acceptance == OwnerAcceptanceStatus::OwnerAccepted
        && implementation.owner_ui_status == OwnerUiStatus::Pending
    {
        return Err("Owner acceptance cannot precede a recorded UI decision or waiver".into());
    }
    if active[0].lifecycle == Lifecycle::Active
        && (implementation.owner_acceptance != OwnerAcceptanceStatus::Pending
            || implementation.owner_ui_status != OwnerUiStatus::Pending)
    {
        return Err("an active phase cannot claim Owner acceptance or UI review".into());
    }
    if active[0].lifecycle == Lifecycle::Active
        && implementation.builder_status == BuilderStatus::NotStarted
        && implementation.automated_status != AutomatedStatus::NotRun
    {
        return Err("a phase with no implementation cannot claim automated acceptance".into());
    }
    if implementation.next_phase_unlocked {
        return Err("implementation state must not unlock a later phase automatically".into());
    }
    Ok(())
}

fn validate_acceptance_gates(phase: &PhaseManifest) -> Result<(), String> {
    if phase.acceptance_gates.is_empty() {
        return Err("phase manifest must declare acceptance gates".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for gate in &phase.acceptance_gates {
        let id = gate.id.trim();
        if id.is_empty()
            || gate.id != id
            || gate.requirement.trim().len() < 20
            || gate.evidence.trim().len() < 20
            || is_placeholder(&gate.requirement)
            || is_placeholder(&gate.evidence)
        {
            return Err("phase manifest contains an acceptance gate without an ID, requirement, or evidence".into());
        }
        if !ids.insert(id) {
            return Err(format!(
                "phase manifest contains duplicate acceptance gate ID: {id}"
            ));
        }
    }
    let required = match phase.phase.as_str() {
        "RG-R00" => Some(REQUIRED_RG_R00_GATES.as_slice()),
        "RG-R01" => Some(REQUIRED_RG_R01_GATES.as_slice()),
        "RG-R02" => Some(REQUIRED_RG_R02_GATES.as_slice()),
        "RG-R03" => Some(REQUIRED_RG_R03_GATES.as_slice()),
        "RG-R04" => Some(REQUIRED_RG_R04_GATES.as_slice()),
        _ => None,
    };
    if let Some(required) = required {
        let required_set: std::collections::BTreeSet<_> = required.iter().copied().collect();
        let missing: Vec<_> = required_set.difference(&ids).copied().collect();
        let unexpected: Vec<_> = ids.difference(&required_set).copied().collect();
        if !missing.is_empty() || !unexpected.is_empty() {
            return Err(format!(
                "{} acceptance gates do not match the mandatory contract (missing: {}; unexpected: {})",
                phase.phase,
                missing.join(", "),
                unexpected.join(", ")
            ));
        }
    }
    Ok(())
}

fn is_placeholder(value: &str) -> bool {
    let normalized = value.trim().to_ascii_uppercase();
    ["TODO", "TBD", "PLACEHOLDER", "TBA", "N/A"]
        .iter()
        .any(|marker| normalized == *marker || normalized.starts_with(&format!("{marker}:")))
}

fn validate_acceptance_history(
    roadmap: &Roadmap,
    implementation: &ImplementationState,
    active_index: usize,
) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for record in &implementation.accepted_phases {
        if !seen.insert(record.phase.as_str()) {
            return Err(format!("duplicate acceptance history for {}", record.phase));
        }
        let Some(index) = roadmap
            .phases
            .iter()
            .position(|phase| phase.id == record.phase)
        else {
            return Err(format!(
                "acceptance history references unknown phase {}",
                record.phase
            ));
        };
        if index >= active_index || roadmap.phases[index].lifecycle != Lifecycle::Accepted {
            return Err(format!(
                "acceptance history for {} is not a prior accepted phase",
                record.phase
            ));
        }
        if record.owner_acceptance != OwnerAcceptanceStatus::OwnerAccepted {
            return Err(format!(
                "accepted phase {} lacks Owner acceptance",
                record.phase
            ));
        }
        if record.source_sha.len() != 40
            || !record
                .source_sha
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !record.ci_run_url.starts_with("https://github.com/")
            || !record.owner_issue_url.starts_with("https://github.com/")
        {
            return Err(format!(
                "acceptance evidence for {} is incomplete or malformed",
                record.phase
            ));
        }
        if record.source_tree_sha.as_ref().is_some_and(|tree_sha| {
            tree_sha.len() != 40 || !tree_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(format!(
                "acceptance tree SHA for {} is malformed",
                record.phase
            ));
        }
        let evidence = roadmap.phases[index]
            .acceptance_evidence
            .as_deref()
            .unwrap_or_default();
        if !evidence.contains(&record.source_sha)
            || !evidence.contains(&record.ci_run_url)
            || !evidence.contains(&record.owner_issue_url)
            || record
                .source_tree_sha
                .as_deref()
                .is_some_and(|tree_sha| !evidence.contains(tree_sha))
        {
            return Err(format!(
                "roadmap acceptance evidence does not match the recorded evidence for {}",
                record.phase
            ));
        }
        if record.owner_ui_status == OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
            && !evidence.to_ascii_lowercase().contains("waived, not tested")
        {
            return Err(format!(
                "UI waiver for {} must state that review was not tested",
                record.phase
            ));
        }
        if record.phase == "RG-R01" {
            if record.source_tree_sha.is_none() {
                return Err("RG-R01 acceptance evidence requires the exact source tree SHA".into());
            }
            if !record.known_debts.iter().any(|debt| {
                let normalized = debt.to_ascii_lowercase();
                normalized.contains("cancelpreview") && normalized.contains("refresh")
            }) {
                return Err("RG-R01 acceptance must preserve the cancel-to-refresh UX debt".into());
            }
        }
        if record.phase == "RG-R02" {
            if record.source_tree_sha.is_none() {
                return Err("RG-R02 acceptance evidence requires the exact source tree SHA".into());
            }
            if !record.known_debts.iter().any(|debt| {
                let normalized = debt.to_ascii_lowercase();
                normalized.contains("visual") && normalized.contains("deferred")
            }) {
                return Err("RG-R02 acceptance must preserve the deferred visual UX debt".into());
            }
        }
        if record.phase == "RG-R03" {
            if record.source_tree_sha.is_none() {
                return Err("RG-R03 acceptance evidence requires the exact source tree SHA".into());
            }
            if !record.known_debts.iter().any(|debt| {
                let normalized = debt.to_ascii_lowercase();
                normalized.contains("schema") && normalized.contains("not authorized")
            }) {
                return Err(
                    "RG-R03 acceptance must preserve the unauthorized Owner database upgrade"
                        .into(),
                );
            }
        }
    }
    for phase in roadmap.phases.iter().take(active_index) {
        if phase.lifecycle == Lifecycle::Accepted && !seen.contains(phase.id.as_str()) {
            return Err(format!(
                "accepted phase {} has no structured Owner evidence",
                phase.id
            ));
        }
    }
    Ok(())
}

fn snapshot(documents: Documents) -> Result<GovernanceSnapshot, String> {
    let active = documents
        .roadmap
        .phases
        .iter()
        .find(|phase| phase.id == documents.roadmap.current_phase)
        .ok_or_else(|| "current phase missing".to_owned())?;
    let page_phases = [
        ("data-ground", "RG-R01"),
        ("feature-studio", "RG-R02"),
        ("strategy-studio", "RG-R03"),
        ("model-lab", "RG-R04"),
        ("policy-studio", "RG-R05"),
        ("experiments", "RG-R06"),
        ("results", "RG-R07"),
    ];
    let builder_status = documents.implementation.builder_status;
    let current_phase = documents.roadmap.current_phase.as_str();
    let pages = std::iter::once(PageSnapshot {
        id: "overview",
        status: "IMPLEMENTED".into(),
    })
    .chain(page_phases.into_iter().map(|(id, phase_id)| {
        let lifecycle = documents
            .roadmap
            .phases
            .iter()
            .find(|phase| phase.id == phase_id)
            .map(|phase| phase.lifecycle)
            .unwrap_or(Lifecycle::Planned);
        let status = if lifecycle == Lifecycle::Active
            && phase_id == current_phase
            && builder_status == BuilderStatus::NotStarted
        {
            "ACTIVE_NOT_STARTED"
        } else {
            match lifecycle {
                Lifecycle::Active => "IN_PROGRESS",
                Lifecycle::Accepted => "OWNER_ACCEPTED",
                Lifecycle::Planned => "PLANNED",
            }
        };
        PageSnapshot {
            id,
            status: status.into(),
        }
    }))
    .collect();
    Ok(GovernanceSnapshot {
        project: documents.roadmap.project,
        current_phase: documents.roadmap.current_phase,
        phase_lifecycle: active.lifecycle,
        builder_status: documents.implementation.builder_status,
        automated_status: documents.implementation.automated_status,
        owner_ui_status: documents.implementation.owner_ui_status,
        owner_acceptance: documents.implementation.owner_acceptance,
        next_phase_unlocked: documents.implementation.next_phase_unlocked,
        deliverables: documents.implementation.deliverables,
        accepted_phases: documents.implementation.accepted_phases,
        pages,
    })
}

pub fn write_roadmap_document(root: impl AsRef<Path>) -> Result<(), String> {
    let root = root.as_ref();
    let roadmap_source =
        fs::read_to_string(root.join("ROADMAP.yaml")).map_err(|e| e.to_string())?;
    let roadmap: Roadmap = serde_yaml_ng::from_str(&roadmap_source).map_err(|e| e.to_string())?;
    fs::write(root.join("ROADMAP.md"), render_roadmap(&roadmap)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rg_r01_test_manifest() -> PhaseManifest {
        accepted_test_manifest("RG-R01", &REQUIRED_RG_R01_GATES)
    }

    fn accepted_test_manifest(phase: &str, gates: &[&str]) -> PhaseManifest {
        PhaseManifest {
            schema_version: 3,
            phase: phase.into(),
            lifecycle: Lifecycle::Accepted,
            deliverables: vec!["accepted_phase".into()],
            acceptance_gates: gates
                .iter()
                .map(|id| AcceptanceGate {
                    id: (*id).into(),
                    requirement: "A sufficiently detailed mandatory acceptance requirement.".into(),
                    evidence: "A sufficiently detailed validation evidence contract.".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn embedded_roadmap_phase_manifest_and_implementation_state_are_synchronized() {
        let result = validate_embedded().unwrap();
        assert_eq!(result.current_phase, "RG-R04");
        assert_eq!(result.phase_lifecycle, Lifecycle::Active);
        assert_eq!(result.builder_status, BuilderStatus::Implemented);
        assert_eq!(result.automated_status, AutomatedStatus::AutomatedPass);
        assert_eq!(result.owner_ui_status, OwnerUiStatus::Pending);
        assert_eq!(result.owner_acceptance, OwnerAcceptanceStatus::Pending);
        assert!(!result.next_phase_unlocked);
        assert_eq!(result.pages.len(), 8);
        assert_eq!(
            result
                .pages
                .iter()
                .filter(|page| page.status == "PLANNED")
                .count(),
            3
        );
        let page_status = |id: &str| {
            result
                .pages
                .iter()
                .find(|page| page.id == id)
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(page_status("data-ground"), "OWNER_ACCEPTED");
        assert_eq!(page_status("feature-studio"), "OWNER_ACCEPTED");
        assert_eq!(page_status("strategy-studio"), "OWNER_ACCEPTED");
        assert_eq!(page_status("model-lab"), "IN_PROGRESS");
        assert_eq!(result.accepted_phases.len(), 4);
        let accepted_r03 = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R03")
            .unwrap();
        assert_eq!(
            accepted_r03.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert_eq!(
            accepted_r03.source_sha,
            "3d3ceeb34958130f1e3f67309d0a213d10c971c3"
        );
        assert_eq!(
            accepted_r03.source_tree_sha.as_deref(),
            Some("4e8f1542a1d6a3872219c1dad9747dde5841bd88")
        );
        assert_eq!(accepted_r03.known_debts.len(), 3);
        let accepted_r02 = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R02")
            .unwrap();
        assert_eq!(
            accepted_r02.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert_eq!(
            accepted_r02.source_sha,
            "59e0f2e7bd5a875b661bb608d289b3b21b595bd2"
        );
        assert_eq!(
            accepted_r02.source_tree_sha.as_deref(),
            Some("326b01e72b915746be2134d00a2d0d1fa737d853")
        );
        assert_eq!(accepted_r02.known_debts.len(), 2);
        let accepted_r01 = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R01")
            .unwrap();
        assert_eq!(
            accepted_r01.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert_eq!(
            accepted_r01.source_sha,
            "7f50b398ca272c4c84b5e5c01866945e9eda2a2a"
        );
        assert_eq!(
            accepted_r01.source_tree_sha.as_deref(),
            Some("46c8969bc31faa7f3efaa6474d5ced2322270623")
        );
        assert_eq!(accepted_r01.known_debts.len(), 1);
    }

    #[test]
    fn roadmap_has_exactly_one_active_phase_and_cannot_skip_a_gate() {
        let changed = ROADMAP_SOURCE.replace("lifecycle: PLANNED", "lifecycle: ACTIVE");
        assert!(parse_documents(&changed, PHASE_SOURCE, IMPLEMENTATION_SOURCE).is_err());
    }

    #[test]
    fn phase_manifest_must_match_the_active_roadmap_phase() {
        let changed = PHASE_SOURCE.replace("phase: RG-R04", "phase: RG-R05");
        assert!(parse_documents(ROADMAP_SOURCE, &changed, IMPLEMENTATION_SOURCE).is_err());
    }

    #[test]
    fn gate_evidence_and_ci_state_require_their_new_schema_versions() {
        let old_phase_schema = PHASE_SOURCE.replacen("schema_version: 3", "schema_version: 1", 1);
        assert!(parse_documents(ROADMAP_SOURCE, &old_phase_schema, IMPLEMENTATION_SOURCE).is_err());

        let old_implementation_schema =
            IMPLEMENTATION_SOURCE.replacen("schema_version: 3", "schema_version: 1", 1);
        assert!(parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &old_implementation_schema).is_err());
    }

    #[test]
    fn implementation_deliverables_must_match_phase_manifest() {
        let changed = IMPLEMENTATION_SOURCE
            .lines()
            .filter(|line| !line.trim_start().starts_with("model_discovery:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).is_err());
    }

    #[test]
    fn generated_document_has_a_single_active_phase_statement() {
        let mut roadmap: Roadmap = serde_yaml_ng::from_str(ROADMAP_SOURCE).unwrap();
        let generated = render_roadmap(&roadmap);
        assert!(generated.contains("Current active phase: RG-R04"));
        assert!(generated.contains("RG-R08"));
        assert!(generated.ends_with('\n'));
        assert!(!generated.ends_with("\n\n"));

        roadmap.current_phase = "RG-R05".into();
        for phase in &mut roadmap.phases {
            match phase.id.as_str() {
                "RG-R00" | "RG-R01" | "RG-R02" | "RG-R03" | "RG-R04" => {
                    phase.lifecycle = Lifecycle::Accepted;
                    phase.acceptance_evidence = Some("fixture evidence".into());
                }
                "RG-R05" => {
                    phase.lifecycle = Lifecycle::Active;
                    phase.acceptance_evidence = None;
                }
                _ => {
                    phase.lifecycle = Lifecycle::Planned;
                    phase.acceptance_evidence = None;
                }
            }
        }
        let generated_later_phase = render_roadmap(&roadmap);
        assert!(generated_later_phase.contains("Current active phase: RG-R05"));
        assert!(generated_later_phase.contains("## RG-R05 — Policy DSL and Extensibility (ACTIVE)"));
        assert!(generated_later_phase
            .contains("## RG-R00 — Rust foundation, Tauri shell, governance, CI (ACCEPTED)"));
        assert!(!generated_later_phase.contains("RG-R00 is the only active phase"));
    }

    #[test]
    fn rg_r01_gate_contract_remains_fixed_after_transition() {
        let mut phase = rg_r01_test_manifest();
        phase
            .acceptance_gates
            .retain(|gate| gate.id != "owner_ui_acceptance");
        let error = validate_acceptance_gates(&phase).unwrap_err();
        assert!(error.contains("owner_ui_acceptance"));
    }

    #[test]
    fn rg_r01_gate_set_rejects_unexpected_and_unsubstantiated_gates() {
        let mut phase = rg_r01_test_manifest();
        phase.acceptance_gates.push(AcceptanceGate {
            id: "future_gate".into(),
            requirement: "This gate has enough text to be a real requirement.".into(),
            evidence: "This evidence has enough text to be a real validation claim.".into(),
        });
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("unexpected"));

        let mut phase: PhaseManifest = serde_yaml_ng::from_str(PHASE_SOURCE).unwrap();
        phase.acceptance_gates[0].evidence = "TODO".into();
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("without an ID, requirement, or evidence"));
    }

    #[test]
    fn rg_r02_gate_contract_remains_fixed_after_transition() {
        let mut phase = accepted_test_manifest("RG-R02", &REQUIRED_RG_R02_GATES);
        assert!(validate_acceptance_gates(&phase).is_ok());
        phase
            .acceptance_gates
            .retain(|gate| gate.id != "indicator_registry");
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("indicator_registry"));
    }

    #[test]
    fn rg_r03_gate_contract_remains_fixed_after_transition() {
        let mut phase = accepted_test_manifest("RG-R03", &REQUIRED_RG_R03_GATES);
        assert!(validate_acceptance_gates(&phase).is_ok());
        phase
            .acceptance_gates
            .retain(|gate| gate.id != "decision_semantics");
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("decision_semantics"));
    }

    #[test]
    fn rg_r04_gate_contract_is_exact_and_cannot_lose_a_mandatory_gate() {
        let mut phase: PhaseManifest = serde_yaml_ng::from_str(PHASE_SOURCE).unwrap();
        assert_eq!(phase.phase, "RG-R04");
        assert!(validate_acceptance_gates(&phase).is_ok());
        phase
            .acceptance_gates
            .retain(|gate| gate.id != "model_authority_boundary");
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("model_authority_boundary"));
    }

    #[test]
    fn accepted_history_requires_owner_issue_sha_and_ci_evidence() {
        let changed = IMPLEMENTATION_SOURCE.replace(
            "source_sha: a1f7ada003d57a26dcd4dfa41e86ba62a5890bed",
            "source_sha: invalid",
        );
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).unwrap_err();
        assert!(error.contains("incomplete or malformed"));
    }

    #[test]
    fn a_waived_ui_review_is_recorded_as_waived_not_passed() {
        let result = validate_embedded().unwrap();
        let accepted = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R01")
            .unwrap();
        assert_eq!(
            accepted.owner_acceptance,
            OwnerAcceptanceStatus::OwnerAccepted
        );
        assert_eq!(
            accepted.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert_ne!(accepted.owner_ui_status, OwnerUiStatus::OwnerAccepted);
        assert!(accepted.known_debts[0]
            .to_ascii_lowercase()
            .contains("cancelpreview"));
    }

    #[test]
    fn rg_r01_known_cancel_to_refresh_debt_cannot_be_dropped_from_acceptance_history() {
        let changed = IMPLEMENTATION_SOURCE.replace(
            "cancelPreview may report a generic error",
            "an unrelated display issue may report a generic error",
        );
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).unwrap_err();
        assert!(error.contains("cancel-to-refresh UX debt"));
    }

    #[test]
    fn rg_r02_acceptance_records_waived_ui_review_and_deferred_visual_debt() {
        let result = validate_embedded().unwrap();
        let accepted = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R02")
            .unwrap();
        assert_eq!(
            accepted.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert!(accepted
            .known_debts
            .iter()
            .any(|debt| debt.contains("NOT_RUN")));

        let dropped = IMPLEMENTATION_SOURCE.replace("redesign is deferred", "is acceptable");
        assert_ne!(dropped, IMPLEMENTATION_SOURCE);
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &dropped).unwrap_err();
        assert!(error.contains("deferred visual UX debt"));

        let without_tree = IMPLEMENTATION_SOURCE.replace(
            "    source_tree_sha: 326b01e72b915746be2134d00a2d0d1fa737d853\n",
            "",
        );
        assert_ne!(without_tree, IMPLEMENTATION_SOURCE);
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &without_tree).unwrap_err();
        assert!(error.contains("RG-R02 acceptance evidence requires the exact source tree SHA"));
    }

    #[test]
    fn rg_r03_acceptance_records_waived_ui_review_and_unauthorized_owner_database_upgrade() {
        let result = validate_embedded().unwrap();
        let accepted = result
            .accepted_phases
            .iter()
            .find(|phase| phase.phase == "RG-R03")
            .unwrap();
        assert_eq!(
            accepted.owner_ui_status,
            OwnerUiStatus::OwnerAcceptedWithUiReviewWaived
        );
        assert!(accepted
            .known_debts
            .iter()
            .any(|debt| debt.contains("NOT_RUN")));

        let dropped = IMPLEMENTATION_SOURCE.replace(
            "is NOT AUTHORIZED by this acceptance",
            "was approved with this acceptance",
        );
        assert_ne!(dropped, IMPLEMENTATION_SOURCE);
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &dropped).unwrap_err();
        assert!(error.contains("unauthorized Owner database upgrade"));

        let without_tree = IMPLEMENTATION_SOURCE.replace(
            "    source_tree_sha: 4e8f1542a1d6a3872219c1dad9747dde5841bd88\n",
            "",
        );
        assert_ne!(without_tree, IMPLEMENTATION_SOURCE);
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &without_tree).unwrap_err();
        assert!(error.contains("RG-R03 acceptance evidence requires the exact source tree SHA"));
    }

    #[test]
    fn not_started_phase_cannot_claim_automated_pass() {
        let changed = IMPLEMENTATION_SOURCE
            .replace("builder_status: IMPLEMENTED", "builder_status: NOT_STARTED");
        assert_ne!(changed, IMPLEMENTATION_SOURCE);
        let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).unwrap_err();
        assert!(error.contains("cannot claim automated acceptance"));
    }

    #[test]
    fn rg_r04_builder_cannot_self_accept_ui_or_owner_review_or_unlock_rg_r05() {
        for (from, to, expected) in [
            (
                "owner_ui_status: PENDING",
                "owner_ui_status: OWNER_ACCEPTED",
                "cannot claim Owner acceptance or UI review",
            ),
            (
                "owner_acceptance: PENDING",
                "owner_acceptance: OWNER_ACCEPTED",
                "Owner acceptance cannot precede",
            ),
            (
                "next_phase_unlocked: false",
                "next_phase_unlocked: true",
                "must not unlock a later phase",
            ),
        ] {
            assert!(IMPLEMENTATION_SOURCE.contains(from), "{from}");
            let changed = IMPLEMENTATION_SOURCE.replacen(from, to, 1);
            let error = parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).unwrap_err();
            assert!(error.contains(expected), "{to}: {error}");
        }
        // The first PLANNED lifecycle in the roadmap is RG-R05's.
        let activated = ROADMAP_SOURCE.replacen("lifecycle: PLANNED", "lifecycle: ACTIVE", 1);
        assert_ne!(activated, ROADMAP_SOURCE);
        assert!(parse_documents(&activated, PHASE_SOURCE, IMPLEMENTATION_SOURCE).is_err());
    }

    #[test]
    fn acceptance_gate_ids_must_be_unique() {
        let mut phase: PhaseManifest = serde_yaml_ng::from_str(PHASE_SOURCE).unwrap();
        let duplicate = phase
            .acceptance_gates
            .iter()
            .find(|gate| gate.id == "model_discovery")
            .unwrap()
            .clone();
        phase.acceptance_gates.push(duplicate);
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("duplicate acceptance gate ID"));
    }

    #[test]
    fn every_acceptance_gate_requires_evidence() {
        let mut phase: PhaseManifest = serde_yaml_ng::from_str(PHASE_SOURCE).unwrap();
        phase.acceptance_gates[0].evidence.clear();
        assert!(validate_acceptance_gates(&phase)
            .unwrap_err()
            .contains("evidence"));
    }

    #[test]
    fn embedded_sources_validate_at_build_revision() {
        assert!(validate_project(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).is_ok());
    }

    #[test]
    fn ci_workflow_is_valid_yaml_and_uses_unique_step_and_evidence_keys() {
        let source = include_str!("../../../.github/workflows/research-ground.yml");
        let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(source).unwrap();
        let jobs_key = serde_yaml_ng::Value::String("jobs".into());
        let jobs = document
            .as_mapping()
            .and_then(|mapping| mapping.get(&jobs_key))
            .and_then(serde_yaml_ng::Value::as_mapping)
            .expect("workflow declares a jobs mapping");
        assert!(jobs.contains_key(serde_yaml_ng::Value::String("windows-foundation".into())));

        let step_ids: Vec<_> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("id: "))
            .collect();
        assert_eq!(
            step_ids
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            step_ids.len(),
            "workflow step IDs must be unique"
        );

        let evidence_step = source
            .split("name: Write source-bound acceptance evidence")
            .nth(1)
            .expect("workflow writes source-bound evidence");
        let evidence_environment = evidence_step
            .split("run: ./scripts/New-CiEvidence.ps1")
            .next()
            .expect("evidence writer has an environment block");
        let environment_keys: Vec<_> = evidence_environment
            .lines()
            .filter_map(|line| {
                let key = line.trim().split_once(':')?.0;
                (key.starts_with("RG_STEP_") || key.starts_with("RG_GATE_")).then_some(key)
            })
            .collect();
        assert_eq!(
            environment_keys
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            environment_keys.len(),
            "evidence environment keys must be unique"
        );
    }
}
