export type DeliveryStatus =
  | "PLANNED"
  | "IMPLEMENTED"
  | "TESTED"
  | "VERIFIED"
  | "BLOCKED"
  | "OWNER_ACCEPTED";

export interface PageSnapshot {
  id: string;
  status: "IMPLEMENTED" | "IN_PROGRESS" | "ACTIVE_NOT_STARTED" | "PLANNED" | "OWNER_ACCEPTED";
}

export interface PhaseAcceptanceRecord {
  phase: string;
  owner_acceptance: "PENDING" | "OWNER_ACCEPTED";
  owner_ui_status: "PENDING" | "OWNER_ACCEPTED" | "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED";
  source_sha: string;
  ci_run_url: string;
  owner_issue_url: string;
  source_tree_sha?: string;
  known_debts?: string[];
}

export interface FoundationSnapshot {
  project: string;
  current_phase: string;
  phase_lifecycle: "ACTIVE" | "PLANNED" | "ACCEPTED";
  builder_status: "NOT_STARTED" | "IN_PROGRESS" | "IMPLEMENTED";
  automated_status:
    | "NOT_RUN"
    | "AUTOMATED_PASS"
    | "ENVIRONMENT_BLOCKED"
    | "FAILED";
  owner_ui_status: "PENDING" | "OWNER_ACCEPTED" | "OWNER_ACCEPTED_WITH_UI_REVIEW_WAIVED";
  owner_acceptance: "PENDING" | "OWNER_ACCEPTED";
  next_phase_unlocked: boolean;
  deliverables: Record<string, DeliveryStatus>;
  accepted_phases: PhaseAcceptanceRecord[];
  pages: PageSnapshot[];
}
