import type { SeriesSelector } from "./feature-studio-types";

export interface BindingSpec {
  role: string;
  description: string;
  source: "TARGET" | "REFERENCE";
}

export interface StrategyParameterSpec {
  name: string;
  description: string;
  min: number;
  max: number;
  default: number;
}

export interface StrategyDefinition {
  id: string;
  version: number;
  name: string;
  description: string;
  decision_rule: string;
  equality_semantics: string;
  warmup_rule: string;
  bindings: BindingSpec[];
  parameters: StrategyParameterSpec[];
}

export interface DecisionSemantics {
  version: number;
  decision_rows: string;
  causality: string;
  context: string;
  skip_precedence: string;
  equality: string;
  conflict: string;
}

export interface StrategyRegistrySnapshot {
  engineId: string;
  engineVersion: string;
  decisionSemantics: DecisionSemantics;
  strategies: StrategyDefinition[];
  previewRows: number;
  researchNotice: string;
}

export interface CandidateFeature {
  key: string;
  indicator_id: string;
  source: "TARGET" | "REFERENCE";
  reference: SeriesSelector | null;
  outputs: string[];
}

export interface DecisionInputCandidate {
  feature_set_id: string;
  status: "READY" | "BLOCKED" | "QUARANTINED";
  eligible: boolean;
  reason: string | null;
  target: SeriesSelector | null;
  row_count: number | null;
  created_at_utc: string | null;
  features: CandidateFeature[];
}

export interface FeatureBinding {
  feature_key: string;
  output: string;
}

export interface DecisionSetRequest {
  feature_set_id: string;
  strategy_id: string;
  strategy_version: number;
  parameters: Record<string, number>;
  bindings: Record<string, FeatureBinding>;
  quotas: { max_rows: number; max_output_bytes: number };
}

export type Decision = "BUY" | "SELL" | "SKIP";

export interface PreviewInput {
  role: string;
  status: string;
  value: number | null;
}

export interface PreviewDecision {
  open_time_utc: string;
  decision_time_utc: string;
  decision: Decision;
  reason_code: string;
  reason_role: string | null;
  inputs: PreviewInput[];
}

export interface DecisionSummary {
  buy_rows: number;
  sell_rows: number;
  skip_rows: number;
  skip_reasons: Record<string, number>;
  first_decision_utc: string | null;
  last_decision_utc: string | null;
}

export interface NormalizedBinding {
  role: string;
  source: string;
  feature_key: string;
  output: string;
}

export interface DecisionSetManifest {
  schema_version: number;
  decision_set_id: string;
  identity_sha256: string;
  identity: {
    schema_version: number;
    engine_id: string;
    engine_version: string;
    decision_semantics: DecisionSemantics;
    strategy: {
      strategy_id: string;
      strategy_version: number;
      parameters: Record<string, number>;
      bindings: NormalizedBinding[];
    };
    feature_set: {
      feature_set_id: string;
      identity_sha256: string;
      manifest_sha256: string;
      output_sha256: string;
      output_fingerprint: string;
      row_count: number;
      target: SeriesSelector;
    };
  };
  created_at_utc: string;
  output_file: string;
  output_sha256: string;
  output_fingerprint: string;
  row_count: number;
  summary: DecisionSummary;
  lineage: {
    feature_set_id: string;
    target: SeriesSelector;
    context_series: SeriesSelector[];
    datasets: { dataset_id: string; source_id: string; source_file_name: string; fingerprint: string }[];
  };
  status: "READY" | "QUARANTINED";
  research_notice: string;
}

export interface DecisionSetPreview {
  decision_set_id: string;
  manifest_sha256: string;
  manifest: DecisionSetManifest;
  sample_rows: PreviewDecision[];
}

export interface DecisionPreviewSession {
  jobId: string;
  preview: DecisionSetPreview;
}

export interface DecisionProgressSnapshot {
  phase: "QUEUED" | "VERIFYING_INPUT" | "EVALUATING" | "FINALIZING";
  processed_rows: number;
  total_rows: number;
}

export type ActiveDecisionJob =
  | { state: "RUNNING"; jobId: string; progress: DecisionProgressSnapshot }
  | { state: "PREPARED"; session: DecisionPreviewSession };

export interface DecisionCommitResult {
  decision_set_id: string;
  status: "READY" | "QUARANTINED";
  disposition: "MATERIALIZED" | "REUSED";
}

export interface DecisionSetCatalogEntry {
  decision_set_id: string;
  identity_sha256: string;
  manifest_sha256: string;
  feature_set_id: string;
  status: "READY" | "QUARANTINED";
  manifest: DecisionSetManifest | null;
  integrity: { manifest_valid: boolean; output_valid: boolean; lineage_valid: boolean };
  diagnostic: string | null;
}

export interface DecisionAttempt {
  attempt_id: string;
  started_at_utc: string;
  completed_at_utc: string;
  status: "MATERIALIZED" | "REUSED" | "FAILED" | "CANCELLED";
  decision_set_id: string | null;
  diagnostic_code: string | null;
  diagnostic_message: string | null;
}
