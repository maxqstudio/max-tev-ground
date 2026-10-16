export type DeliveryStatus =
  | "PLANNED"
  | "IMPLEMENTED"
  | "TESTED"
  | "VERIFIED"
  | "BLOCKED"
  | "OWNER_ACCEPTED";

export interface PageSnapshot {
  id: string;
  status: "IMPLEMENTED" | "PLANNED";
}

export interface FoundationSnapshot {
  project: string;
  current_phase: string;
  phase_lifecycle: "ACTIVE" | "PLANNED" | "ACCEPTED";
  builder_status: "IN_PROGRESS" | "IMPLEMENTED";
  automated_status:
    | "NOT_RUN"
    | "AUTOMATED_PASS"
    | "ENVIRONMENT_BLOCKED"
    | "FAILED";
  owner_ui_status: "PENDING" | "OWNER_ACCEPTED";
  owner_acceptance: "PENDING" | "OWNER_ACCEPTED";
  ci_status: "NOT_CONFIGURED" | "PENDING" | "PASS" | "FAILED";
  next_phase_unlocked: boolean;
  deliverables: Record<string, DeliveryStatus>;
  pages: PageSnapshot[];
}