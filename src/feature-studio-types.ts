import type { DatasetStatus, SeriesSummary } from "./data-ground-types";

export type ParameterValue = number | string;

export type ParameterSpec = { name: string; description: string } & (
  | { kind: "INTEGER"; min: number; max: number; default: number }
  | { kind: "NUMBER"; min: number; max: number; default: number }
  | { kind: "CHOICE"; options: string[]; default: string }
);

export interface IndicatorDefinition {
  id: string;
  version: number;
  name: string;
  description: string;
  formula: string;
  warmup_rule: string;
  inputs: "SELECTED_PRICE" | "HIGH_LOW_CLOSE" | "HIGH_LOW";
  outputs: string[];
  parameters: ParameterSpec[];
}

export interface TemporalSemantics {
  version: number;
  decision_boundary: string;
  current_bar: string;
  window_basis: string;
  warmup: string;
  reference_join: string;
  reference_staleness: string;
  gaps: string;
}

export interface FeatureRegistrySnapshot {
  engineId: string;
  engineVersion: string;
  temporalSemantics: TemporalSemantics;
  indicators: IndicatorDefinition[];
  limits: {
    maxFeatures: number;
    maxPeriod: number;
    maxStalenessSeconds: number;
    maxAvailabilityLagSeconds: number;
    previewRows: number;
  };
  researchNotice: string;
}

export interface FeatureInputCandidate {
  dataset_id: string;
  status: DatasetStatus;
  eligible: boolean;
  reason: string | null;
  source_file_name: string | null;
  source_id: string | null;
  broker: string | null;
  server: string | null;
  created_at_utc: string | null;
  series: SeriesSummary[];
}

export interface SeriesSelector {
  dataset_id: string;
  symbol: string;
  timeframe: string;
}

export type FeatureSource =
  | { kind: "TARGET" }
  | { kind: "REFERENCE"; series: SeriesSelector; max_staleness_seconds: number; availability_lag_seconds: number };

export interface FeatureSpec {
  key: string;
  indicator_id: string;
  indicator_version: number;
  parameters: Record<string, ParameterValue>;
  source: FeatureSource;
}

export interface FeatureSetRequest {
  target: SeriesSelector;
  features: FeatureSpec[];
  quotas: { max_target_rows: number; max_input_rows: number; max_output_bytes: number };
}

export type RowStatus = "READY" | "WARMUP" | "STALE" | "NO_OBSERVATION";
export type FeatureSetStatus = "READY" | "BLOCKED" | "QUARANTINED";

export interface NormalizedFeature {
  key: string;
  indicator_id: string;
  indicator_version: number;
  parameters: Record<string, ParameterValue>;
  source: FeatureSource;
  outputs: string[];
  warmup_bars: number;
}

export interface InputDatasetIdentity {
  dataset_id: string;
  fingerprint: string;
  canonical_bars_sha256: string;
  canonical_bars_fingerprint: string;
  bar_count: number;
}

export interface FeatureSummary {
  key: string;
  ready_rows: number;
  warmup_rows: number;
  stale_rows: number;
  no_observation_rows: number;
  first_ready_decision_utc: string | null;
}

export interface InputLineage {
  dataset_id: string;
  fingerprint: string;
  source_id: string;
  broker: string;
  server: string;
  source_file_name: string;
  dataset_created_at_utc: string;
  series_used: string[];
}

export interface FeatureSetManifest {
  schema_version: number;
  feature_set_id: string;
  identity_sha256: string;
  identity: {
    schema_version: number;
    engine_id: string;
    engine_version: string;
    temporal_semantics: TemporalSemantics;
    target: SeriesSelector;
    features: NormalizedFeature[];
    inputs: InputDatasetIdentity[];
  };
  created_at_utc: string;
  output_file: string;
  output_sha256: string;
  output_fingerprint: string;
  row_count: number;
  columns: { name: string; kind: string; feature_key: string | null }[];
  feature_summaries: FeatureSummary[];
  lineage: InputLineage[];
  status: FeatureSetStatus;
  research_notice: string;
}

export interface PreviewRow {
  open_time_utc: string;
  decision_time_utc: string;
  features: { key: string; status: RowStatus; values: (number | null)[]; source_available_at_utc: string | null }[];
}

export interface FeatureSetPreview {
  feature_set_id: string;
  manifest_sha256: string;
  manifest: FeatureSetManifest;
  sample_rows: PreviewRow[];
}

export interface FeaturePreviewSession {
  jobId: string;
  preview: FeatureSetPreview;
}

export interface ProgressSnapshot {
  phase: "QUEUED" | "VERIFYING_INPUTS" | "COMPUTING" | "FINALIZING";
  processed_rows: number;
  total_rows: number;
}

export type ActiveFeatureJob =
  | { state: "RUNNING"; jobId: string; progress: ProgressSnapshot }
  | { state: "PREPARED"; session: FeaturePreviewSession };

export interface FeatureCommitResult {
  feature_set_id: string;
  status: FeatureSetStatus;
  disposition: "MATERIALIZED" | "REUSED";
}

export interface FeatureSetCatalogEntry {
  feature_set_id: string;
  identity_sha256: string;
  manifest_sha256: string;
  status: FeatureSetStatus;
  manifest: FeatureSetManifest | null;
  integrity: { manifest_valid: boolean; output_valid: boolean; lineage_valid: boolean };
  diagnostic: string | null;
}

export interface FeatureAttempt {
  attempt_id: string;
  started_at_utc: string;
  completed_at_utc: string;
  status: "MATERIALIZED" | "REUSED" | "FAILED" | "CANCELLED";
  feature_set_id: string | null;
  diagnostic_code: string | null;
  diagnostic_message: string | null;
}
