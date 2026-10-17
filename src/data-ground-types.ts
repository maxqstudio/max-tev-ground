export type MarketRole = "TARGET" | "REFERENCE";
export type DatasetStatus = "READY" | "BLOCKED" | "QUARANTINED";

export interface SelectedSource {
  selectionId: string;
  fileName: string;
}

export interface SourceInspection {
  file_name: string;
  extension: string;
  size_bytes: number;
  headers: string[];
  sample_rows: Array<Record<string, string>>;
}

export interface FieldMapping {
  timestamp: string;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  symbol: string | null;
  timeframe: string | null;
}

export interface ImportRequest {
  mapping: FieldMapping;
  timestamp_semantics: "BAR_OPENS_AT" | "BAR_CLOSES_AT";
  naive_timestamp_offset_minutes: number;
  default_symbol: string;
  default_timeframe: string;
  default_market_role: MarketRole;
  market_roles: Record<string, MarketRole>;
  source_id: string;
  broker: string;
  server: string;
  delimiter: "COMMA" | "SEMICOLON" | "TAB";
  calendar:
    | { kind: "UNKNOWN" }
    | { kind: "CONTINUOUS" }
    | { kind: "WEEKENDS_UTC" }
    | { kind: "CUSTOM_CLOSED_INTERVALS"; intervals: Array<{ start_utc: string; end_utc: string }> };
  quotas: {
    max_source_bytes: number;
    max_records: number;
  };
}

export interface GapSample {
  symbol: string;
  timeframe: string;
  first_missing_open_utc: string;
  next_observed_open_utc: string;
  expected_missing_bars: number;
  closed_session_bars: number;
  missing_bars: number;
  unclassified_bars: number;
}

export interface QualityAudit {
  duplicate_bars: number;
  descending_bars: number;
  missing_bars: number;
  closed_session_bars: number;
  unclassified_gap_bars: number;
  gaps: GapSample[];
}

export interface SeriesSummary {
  symbol: string;
  timeframe: string;
  market_role: string;
  bar_count: number;
  first_open_utc: string;
  last_open_utc: string;
  missing_bars: number;
  closed_session_bars: number;
  unclassified_gap_bars: number;
}

export interface ImportPreview {
  dataset_id: string;
  fingerprint: string;
  status: DatasetStatus;
  source_file_name: string;
  source_format: string;
  source_size_bytes: number;
  source_sha256: string;
  raw_sha256: string;
  canonical_bars_sha256: string;
  canonical_bars_fingerprint: string;
  parquet_sha256: string;
  manifest_sha256: string;
  bar_count: number;
  series_count: number;
  series: SeriesSummary[];
  quality_audit: QualityAudit;
}

export interface ImportPreviewSession {
  jobId: string;
  preview: ImportPreview;
}

export interface DatasetManifest {
  schema_version: number;
  dataset_id: string;
  fingerprint: string;
  created_at_utc: string;
  provenance: {
    source_id: string;
    broker: string;
    server: string;
    source_file_name: string;
    source_format: string;
    source_size_bytes: number;
    raw_sha256: string;
  };
  import_request: ImportRequest;
  canonical_bars_file: string;
  canonical_bars_sha256: string;
  canonical_bars_fingerprint: string;
  bar_count: number;
  status: DatasetStatus;
  series: SeriesSummary[];
  quality_audit: QualityAudit;
}

export interface DatasetCatalogEntry {
  dataset_id: string;
  fingerprint: string;
  manifest_sha256: string;
  status: DatasetStatus;
  manifest: DatasetManifest | null;
  integrity: {
    manifest_valid: boolean;
    raw_source_valid: boolean;
    canonical_parquet_valid: boolean;
  };
  diagnostic: string | null;
}

export interface ImportAttempt {
  attempt_id: string;
  started_at_utc: string;
  completed_at_utc: string;
  status: string;
  source_file_name: string;
  diagnostic_code: string | null;
  diagnostic_message: string | null;
}

export interface ImportCommitResult {
  dataset_id: string;
  fingerprint: string;
  status: DatasetStatus;
  disposition: "IMPORTED" | "REUSED";
}
