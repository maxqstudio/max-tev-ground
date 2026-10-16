use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};

const ROADMAP_SOURCE: &str = include_str!("../../../ROADMAP.yaml");
const PHASE_SOURCE: &str = include_str!("../../../PHASE_MANIFEST.yaml");
const IMPLEMENTATION_SOURCE: &str = include_str!("../../../IMPLEMENTATION_STATE.yaml");

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
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationState {
    pub schema_version: u16,
    pub current_phase: String,
    pub builder_status: BuilderStatus,
    pub automated_status: AutomatedStatus,
    pub owner_ui_status: OwnerUiStatus,
    pub owner_acceptance: OwnerUiStatus,
    pub ci_status: CiStatus,
    pub next_phase_unlocked: bool,
    pub deliverables: BTreeMap<String, DeliveryStatus>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BuilderStatus {
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
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CiStatus {
    NotConfigured,
    Pending,
    Pass,
    Failed,
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
    pub owner_acceptance: OwnerUiStatus,
    pub ci_status: CiStatus,
    pub next_phase_unlocked: bool,
    pub deliverables: BTreeMap<String, DeliveryStatus>,
    pub pages: Vec<PageSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageSnapshot {
    pub id: &'static str,
    pub status: &'static str,
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
    let mut output = String::from(
        "# MAX Research Ground roadmap\n\n\
         Generated from ROADMAP.yaml. RG-R00 is the only active phase; later phases stay locked until the Owner accepts the current checkpoint and authorizes progression.\n\n",
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
        || phase.schema_version != 1
        || implementation.schema_version != 1
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
    if phase.phase != roadmap.current_phase || implementation.current_phase != roadmap.current_phase
    {
        return Err(
            "roadmap, phase manifest, and implementation state disagree on current phase".into(),
        );
    }
    if phase.lifecycle != active[0].lifecycle {
        return Err("phase manifest lifecycle is out of sync with roadmap".into());
    }
    if phase.deliverables.is_empty() || phase.acceptance_gates.is_empty() {
        return Err("phase manifest must declare deliverables and acceptance gates".into());
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
    if phase
        .acceptance_gates
        .iter()
        .any(|gate| gate.id.trim().is_empty() || gate.requirement.trim().is_empty())
    {
        return Err("phase manifest contains an empty acceptance gate".into());
    }
    let declared: std::collections::BTreeSet<_> = phase.deliverables.iter().cloned().collect();
    let recorded: std::collections::BTreeSet<_> =
        implementation.deliverables.keys().cloned().collect();
    if declared != recorded {
        return Err("phase manifest and implementation state deliverables are out of sync".into());
    }
    if implementation.schema_version != 1 {
        return Err("unsupported implementation state schema version".into());
    }
    if implementation.owner_acceptance == OwnerUiStatus::OwnerAccepted
        && implementation.owner_ui_status != OwnerUiStatus::OwnerAccepted
    {
        return Err("Owner acceptance cannot precede Owner UI acceptance".into());
    }
    if implementation.next_phase_unlocked {
        return Err("RG-R00 must not unlock RG-R01 automatically".into());
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
    Ok(GovernanceSnapshot {
        project: documents.roadmap.project,
        current_phase: documents.roadmap.current_phase,
        phase_lifecycle: active.lifecycle,
        builder_status: documents.implementation.builder_status,
        automated_status: documents.implementation.automated_status,
        owner_ui_status: documents.implementation.owner_ui_status,
        owner_acceptance: documents.implementation.owner_acceptance,
        ci_status: documents.implementation.ci_status,
        next_phase_unlocked: documents.implementation.next_phase_unlocked,
        deliverables: documents.implementation.deliverables,
        pages: vec![
            PageSnapshot {
                id: "overview",
                status: "IMPLEMENTED",
            },
            PageSnapshot {
                id: "data-ground",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "feature-studio",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "strategy-studio",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "model-lab",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "policy-studio",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "experiments",
                status: "PLANNED",
            },
            PageSnapshot {
                id: "results",
                status: "PLANNED",
            },
        ],
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

    #[test]
    fn embedded_roadmap_phase_manifest_and_implementation_state_are_synchronized() {
        let result = validate_embedded().unwrap();
        assert_eq!(result.current_phase, "RG-R00");
        assert_eq!(result.phase_lifecycle, Lifecycle::Active);
        assert!(!result.next_phase_unlocked);
        assert_eq!(result.pages.len(), 8);
        assert_eq!(
            result
                .pages
                .iter()
                .filter(|page| page.status == "PLANNED")
                .count(),
            7
        );
    }

    #[test]
    fn roadmap_has_exactly_one_active_phase_and_cannot_skip_a_gate() {
        let changed = ROADMAP_SOURCE.replace("lifecycle: PLANNED", "lifecycle: ACTIVE");
        assert!(parse_documents(&changed, PHASE_SOURCE, IMPLEMENTATION_SOURCE).is_err());
    }

    #[test]
    fn phase_manifest_must_match_the_active_roadmap_phase() {
        let changed = PHASE_SOURCE.replace("phase: RG-R00", "phase: RG-R01");
        assert!(parse_documents(ROADMAP_SOURCE, &changed, IMPLEMENTATION_SOURCE).is_err());
    }

    #[test]
    fn implementation_deliverables_must_match_phase_manifest() {
        let changed = IMPLEMENTATION_SOURCE.replace("  desktop_shell: IMPLEMENTED\n", "");
        assert!(parse_documents(ROADMAP_SOURCE, PHASE_SOURCE, &changed).is_err());
    }

    #[test]
    fn generated_document_has_a_single_active_phase_statement() {
        let roadmap: Roadmap = serde_yaml_ng::from_str(ROADMAP_SOURCE).unwrap();
        let generated = render_roadmap(&roadmap);
        assert!(generated.contains("RG-R00 is the only active phase"));
        assert!(generated.contains("RG-R08"));
        assert!(generated.ends_with('\n'));
        assert!(!generated.ends_with("\n\n"));
    }

    #[test]
    fn embedded_sources_validate_at_build_revision() {
        assert!(validate_project(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).is_ok());
    }
}
