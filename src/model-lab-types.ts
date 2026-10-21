import type { SeriesSelector } from "./feature-studio-types";

export type AdapterKind = "OLLAMA_LOOPBACK" | "ONNX_NATIVE";
export type ModelStatus = "READY" | "QUARANTINED" | "RUNTIME_UNAVAILABLE";
export type InputKind = "FEATURE_SET" | "DECISION_SET";
export type Determinism = "NOT_REPLAYED" | "REPRODUCED" | "NON_DETERMINISTIC";

export interface InferenceSemantics {
  version: number;
  rows: string;
  causality: string;
  skip: string;
  invalid_output: string;
  authority: string;
  determinism: string;
}

export interface ModelLabRegistrySnapshot {
  registryVersion: string;
  adapters: { id: AdapterKind; version: string; description: string }[];
  defaultOllamaEndpoint: string;
  onnxOperators: string[];
  onnxOpsets: [number, number];
  maxOnnxBytes: number;
  ollamaOutputTokens: [number, number];
  ollamaContextTokens: [number, number];
  requestTimeoutMs: [number, number];
  executableAuthority: "ADVISORY_ONLY";
  lockedAuthorities: string[];
  inferenceSemantics: InferenceSemantics;
  previewRows: number;
  modelNotice: string;
  inferenceNotice: string;
  comparisonNotice: string;
}

export interface OllamaListing {
  name: string;
  digest: string;
  size_bytes: number | null;
  modified_at: string | null;
  format: string | null;
  family: string | null;
  parameter_size: string | null;
  quantization_level: string | null;
}

export interface OnnxTensorInfo {
  name: string;
  shape: number[];
  width: number;
}

export interface OnnxModelInfo {
  ir_version: number;
  opset: number;
  producer_name: string;
  input: OnnxTensorInfo;
  outputs: OnnxTensorInfo[];
  operators: Record<string, number>;
  parameter_count: number;
  operations_per_row: number;
}

export interface ModelCandidate {
  candidate_id: string;
  adapter: AdapterKind;
  model_name: string;
  content_digest: string;
  eligible: boolean;
  reason: string | null;
  registered_model_ids: string[];
  ollama: OllamaListing | null;
  onnx: OnnxModelInfo | null;
  content_bytes: number | null;
}

export interface OllamaDiscovery {
  endpoint: string;
  availability: "AVAILABLE" | "UNAVAILABLE" | "MALFORMED";
  diagnostic: string | null;
  candidates: ModelCandidate[];
}

export type OutputKind =
  | { kind: "FLOAT64"; minimum: number; maximum: number }
  | { kind: "CATEGORY"; labels: string[] };

export interface OutputField {
  name: string;
  kind: OutputKind;
}

export type RuntimeConfig =
  | { adapter: "OLLAMA_LOOPBACK"; seed: number; max_output_tokens: number; context_tokens: number; instruction: string }
  | { adapter: "ONNX_NATIVE" };

export interface ModelRegistrationRequest {
  candidate_id: string;
  input_fields: string[];
  outputs: OutputField[];
  runtime: RuntimeConfig;
  authority: "ADVISORY_ONLY";
}

export interface ModelIdentity {
  schema_version: number;
  registry_id: string;
  registry_version: string;
  adapter: AdapterKind;
  adapter_version: string;
  model_name: string;
  content_digest: string;
  input_fields: string[];
  outputs: OutputField[];
  runtime: RuntimeConfig;
  decoding: Record<string, unknown>;
  onnx: OnnxModelInfo | null;
  authority: "ADVISORY_ONLY";
}

export interface ModelManifest {
  schema_version: number;
  model_id: string;
  identity_sha256: string;
  identity: ModelIdentity;
  registered_at_utc: string;
  content_file: string | null;
  content_bytes: number | null;
  source: { kind: "OLLAMA_LISTING"; endpoint: string; listing: OllamaListing } | { kind: "ONNX_FILE"; source_file_name: string };
  status: ModelStatus;
  research_notice: string;
}

export interface RegistrationPreview {
  model_id: string;
  manifest: ModelManifest;
  already_registered: boolean;
}

export interface RegistrationResult {
  model_id: string;
  disposition: "REGISTERED" | "ALREADY_REGISTERED";
}

export interface ModelCatalogEntry {
  model_id: string;
  adapter: AdapterKind | null;
  model_name: string | null;
  status: ModelStatus;
  manifest: ModelManifest | null;
  integrity: { manifest_valid: boolean; content_valid: boolean };
  diagnostic: string | null;
}

export type InputColumn =
  | { source: "FEATURE"; feature_key: string; output: string }
  | { source: "DECISION_INPUT"; role: string }
  | { source: "DECISION" };

export interface InputColumnOption {
  binding: InputColumn;
  label: string;
}

export interface InferenceInputCandidate {
  kind: InputKind;
  id: string;
  status: string;
  eligible: boolean;
  reason: string | null;
  target: SeriesSelector | null;
  row_count: number | null;
  created_at_utc: string | null;
  columns: InputColumnOption[];
}

export type InputSetRef =
  | { kind: "FEATURE_SET"; feature_set_id: string }
  | { kind: "DECISION_SET"; decision_set_id: string };

export interface InferenceRequest {
  model_id: string;
  input: InputSetRef;
  bindings: Record<string, InputColumn>;
  authority: "ADVISORY_ONLY";
  quotas: { max_rows: number; max_output_bytes: number; request_timeout_ms: number };
}

export interface InferenceProgressSnapshot {
  phase: "QUEUED" | "VERIFYING_MODEL" | "VERIFYING_INPUT" | "INFERRING" | "FINALIZING";
  processed_rows: number;
  total_rows: number;
}

export interface PreviewValue {
  field: string;
  value: number | null;
  label: string | null;
}

export interface PreviewRow {
  open_time_utc: string;
  decision_time_utc: string;
  row_status: "OK" | "SKIPPED" | "INVALID_OUTPUT";
  reason_code: string | null;
  inputs: PreviewValue[];
  outputs: PreviewValue[];
}

export interface InferenceRecordManifest {
  schema_version: number;
  record_id: string;
  identity_sha256: string;
  identity: {
    model: { model_id: string; identity_sha256: string; adapter: AdapterKind; model_name: string; content_digest: string };
    input: { kind: InputKind; id: string; identity_sha256: string; manifest_sha256: string; output_sha256: string; output_fingerprint: string; row_count: number; target: SeriesSelector };
    bindings: { field: string; source: string; column: string; status_column: string | null }[];
    authority: "ADVISORY_ONLY";
  } & Record<string, unknown>;
  created_at_utc: string;
  output_file: string;
  output_sha256: string;
  output_fingerprint: string;
  row_count: number;
  columns: { name: string; kind: string }[];
  outputs: OutputField[];
  summary: { ok_rows: number; skipped_rows: number; invalid_output_rows: number; reasons: Record<string, number>; first_decision_utc: string | null; last_decision_utc: string | null };
  run: { invoked_rows: number; total_latency_us: number; max_latency_us: number };
  lineage: { model_id: string; model_name: string; adapter: AdapterKind; content_digest: string; input_kind: InputKind; input_id: string; feature_set_id: string; target: SeriesSelector; datasets: unknown[] };
  authority: "ADVISORY_ONLY";
  status: "READY" | "QUARANTINED";
  research_notice: string;
}

export interface InferencePreview {
  record_id: string;
  manifest_sha256: string;
  manifest: InferenceRecordManifest;
  sample_rows: PreviewRow[];
  existing: { record_id: string; output_matches: boolean } | null;
}

export interface InferencePreviewSession {
  jobId: string;
  preview: InferencePreview;
}

export type ActiveInferenceJob =
  | { state: "RUNNING"; jobId: string; progress: InferenceProgressSnapshot }
  | { state: "PREPARED"; session: InferencePreviewSession };

export interface InferenceCommitResult {
  record_id: string;
  disposition: "MATERIALIZED" | "REPLAY_REPRODUCED" | "REPLAY_NON_DETERMINISTIC";
  determinism: Determinism;
}

export interface InferenceReplay {
  replay_id: string;
  record_id: string;
  replayed_at_utc: string;
  output_sha256: string;
  outcome: "REPRODUCED" | "NON_DETERMINISTIC";
}

export interface InferenceRecordCatalogEntry {
  record_id: string;
  model_id: string;
  input_id: string;
  status: "READY" | "QUARANTINED";
  determinism: Determinism;
  replays: InferenceReplay[];
  manifest: InferenceRecordManifest | null;
  integrity: { manifest_valid: boolean; output_valid: boolean; lineage_valid: boolean };
  diagnostic: string | null;
}

export interface ComparisonReport {
  input_kind: InputKind;
  input_id: string;
  input_output_sha256: string;
  rows: number;
  records: {
    record_id: string;
    model_id: string;
    model_name: string;
    rows: number;
    ok_rows: number;
    skipped_rows: number;
    invalid_output_rows: number;
    coverage: number;
    invalid_output_rate: number | null;
    mean_latency_us: number | null;
    max_latency_us: number;
  }[];
  pairs: {
    left_record_id: string;
    right_record_id: string;
    fields: {
      field: string;
      kind: "FLOAT64" | "CATEGORY";
      rows_both_ok: number;
      agreeing_rows: number | null;
      agreement_rate: number | null;
      mean_absolute_difference: number | null;
      max_absolute_difference: number | null;
    }[];
  }[];
  notice: string;
}

export interface ModelLabAttempt {
  attempt_id: string;
  started_at_utc: string;
  completed_at_utc: string;
  operation: "MODEL_REGISTRATION" | "INFERENCE";
  status: string;
  subject_id: string | null;
  diagnostic_code: string | null;
  diagnostic_message: string | null;
}
